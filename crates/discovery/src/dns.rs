//! Minimal DNS wire codec with miekg/dns semantics (Go: github.com/miekg/dns v1.1.43).
//!
//! Names and TXT strings are kept in miekg's presentation format (labels joined by `.`, `\.`,
//! `\ ` and `\DDD` escapes) because libp2p/zeroconf compares and prints those strings directly.
//! Only the record types mDNS service discovery needs are decoded (A, AAAA, PTR, SRV, TXT);
//! anything else is skipped by length.

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};

pub const TYPE_A: u16 = 1;
pub const TYPE_PTR: u16 = 12;
pub const TYPE_TXT: u16 = 16;
pub const TYPE_AAAA: u16 = 28;
pub const TYPE_SRV: u16 = 33;
pub const CLASS_INET: u16 = 1;
/// mDNS cache-flush bit in RR classes and unicast-response bit in question classes.
pub const CLASS_TOP_BIT: u16 = 1 << 15;

const MAX_DOMAIN_NAME_WIRE_OCTETS: i64 = 255;
const MAX_COMPRESSION_POINTERS: usize = (MAX_DOMAIN_NAME_WIRE_OCTETS as usize + 1) / 2 - 2;
const MAX_COMPRESSION_OFFSET: usize = 2 << 13;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("dns: {0}")]
pub struct DnsError(pub &'static str);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub name: String,
    pub qtype: u16,
    pub qclass: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RData {
    A(Ipv4Addr),
    Aaaa(Ipv6Addr),
    Ptr(String),
    Srv { priority: u16, weight: u16, port: u16, target: String },
    Txt(Vec<String>),
    /// A record type this codec does not interpret.
    Other(u16),
}

impl RData {
    fn rtype(&self) -> u16 {
        match self {
            RData::A(_) => TYPE_A,
            RData::Aaaa(_) => TYPE_AAAA,
            RData::Ptr(_) => TYPE_PTR,
            RData::Srv { .. } => TYPE_SRV,
            RData::Txt(_) => TYPE_TXT,
            RData::Other(t) => *t,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rr {
    pub name: String,
    pub class: u16,
    pub ttl: u32,
    pub data: RData,
}

/// A DNS message (Go: `dns.Msg`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Msg {
    pub id: u16,
    pub response: bool,
    pub opcode: u8,
    pub authoritative: bool,
    pub truncated: bool,
    pub recursion_desired: bool,
    pub recursion_available: bool,
    pub zero: bool,
    pub authenticated_data: bool,
    pub checking_disabled: bool,
    pub rcode: u8,
    /// Pack with name compression (Go: `Msg.Compress`).
    pub compress: bool,
    pub question: Vec<Question>,
    pub answer: Vec<Rr>,
    pub ns: Vec<Rr>,
    pub extra: Vec<Rr>,
}

impl Msg {
    /// Go `Msg.SetQuestion`: random id, recursion desired, one IN question.
    pub fn set_question(&mut self, name: &str, qtype: u16) {
        self.id = rand::random();
        self.recursion_desired = true;
        self.question = vec![Question { name: name.to_string(), qtype, qclass: CLASS_INET }];
    }

    /// Go `Msg.SetReply`: a reply header for `request` with no questions kept.
    pub fn reply_to(request: &Msg) -> Msg {
        let mut reply = Msg { id: request.id, response: true, opcode: request.opcode, ..Msg::default() };
        if request.opcode == 0 {
            reply.recursion_desired = request.recursion_desired;
            reply.checking_disabled = request.checking_disabled;
        }
        reply
    }

    /// Go `Msg.Pack`.
    pub fn pack(&self) -> Result<Vec<u8>, DnsError> {
        let mut buf = Vec::with_capacity(512);
        let mut bits = u16::from(self.opcode) << 11 | u16::from(self.rcode & 0xF);
        for (flag, mask) in [
            (self.response, 0x8000),
            (self.authoritative, 0x0400),
            (self.truncated, 0x0200),
            (self.recursion_desired, 0x0100),
            (self.recursion_available, 0x0080),
            (self.zero, 0x0040),
            (self.authenticated_data, 0x0020),
            (self.checking_disabled, 0x0010),
        ] {
            if flag {
                bits |= mask;
            }
        }
        buf.extend_from_slice(&self.id.to_be_bytes());
        buf.extend_from_slice(&bits.to_be_bytes());
        for len in [self.question.len(), self.answer.len(), self.ns.len(), self.extra.len()] {
            let len = u16::try_from(len).map_err(|_| DnsError("too many records"))?;
            buf.extend_from_slice(&len.to_be_bytes());
        }
        let mut comp: Option<HashMap<String, u16>> = self.compress.then(HashMap::new);
        let compress = self.compress;
        for q in &self.question {
            pack_name(&q.name, &mut buf, comp.as_mut(), compress)?;
            buf.extend_from_slice(&q.qtype.to_be_bytes());
            buf.extend_from_slice(&q.qclass.to_be_bytes());
        }
        for rr in self.answer.iter().chain(&self.ns).chain(&self.extra) {
            pack_rr(rr, &mut buf, comp.as_mut(), compress)?;
        }
        if buf.len() > u16::MAX as usize {
            return Err(DnsError("message too large"));
        }
        Ok(buf)
    }

    /// Go `Msg.Unpack`.
    pub fn unpack(msg: &[u8]) -> Result<Msg, DnsError> {
        if msg.len() < 12 {
            return Err(DnsError("overflow unpacking header"));
        }
        let word = |i: usize| u16::from_be_bytes([msg[i], msg[i + 1]]);
        let bits = word(2);
        let mut out = Msg {
            id: word(0),
            response: bits & 0x8000 != 0,
            opcode: ((bits >> 11) & 0xF) as u8,
            authoritative: bits & 0x0400 != 0,
            truncated: bits & 0x0200 != 0,
            recursion_desired: bits & 0x0100 != 0,
            recursion_available: bits & 0x0080 != 0,
            zero: bits & 0x0040 != 0,
            authenticated_data: bits & 0x0020 != 0,
            checking_disabled: bits & 0x0010 != 0,
            rcode: (bits & 0xF) as u8,
            ..Msg::default()
        };
        let (qd, an, ns, ar) = (word(4), word(6), word(8), word(10));
        let mut off = 12;
        if off == msg.len() {
            return Ok(out);
        }
        for _ in 0..qd {
            let before = off;
            if off == msg.len() {
                break;
            }
            let (name, next) = unpack_name(msg, off)?;
            if next + 4 > msg.len() {
                return Err(DnsError("overflow unpacking question"));
            }
            let qtype = u16::from_be_bytes([msg[next], msg[next + 1]]);
            let qclass = u16::from_be_bytes([msg[next + 2], msg[next + 3]]);
            off = next + 4;
            if off == before {
                break;
            }
            out.question.push(Question { name, qtype, qclass });
        }
        out.answer = unpack_rrs(an, msg, &mut off)?;
        out.ns = unpack_rrs(ns, msg, &mut off)?;
        out.extra = unpack_rrs(ar, msg, &mut off)?;
        Ok(out)
    }
}

fn is_fqdn(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.last() != Some(&b'.') {
        return false;
    }
    // A trailing dot preceded by an odd number of backslashes is escaped.
    let backslashes = bytes[..bytes.len() - 1].iter().rev().take_while(|b| **b == b'\\').count();
    backslashes % 2 == 0
}

/// Splits a presentation-format name into raw labels plus the index in `s` where each label
/// starts (compression keys are suffixes of the original string).
fn name_labels(s: &str) -> Result<Vec<(Vec<u8>, usize)>, DnsError> {
    let bytes = s.as_bytes();
    let mut labels: Vec<(Vec<u8>, usize)> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut start = 0usize;
    let mut was_dot = false;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                if i + 3 < bytes.len() && bytes[i + 1..i + 4].iter().all(u8::is_ascii_digit) {
                    let value = u32::from(bytes[i + 1] - b'0') * 100 + u32::from(bytes[i + 2] - b'0') * 10 + u32::from(bytes[i + 3] - b'0');
                    current.push(value as u8);
                    i += 4;
                } else {
                    if let Some(next) = bytes.get(i + 1) {
                        current.push(*next);
                    }
                    i += 2;
                }
                was_dot = false;
            }
            b'.' => {
                if was_dot {
                    return Err(DnsError("bad rdata"));
                }
                was_dot = true;
                if current.len() >= 64 {
                    return Err(DnsError("bad rdata"));
                }
                labels.push((std::mem::take(&mut current), start));
                i += 1;
                start = i;
            }
            other => {
                current.push(other);
                was_dot = false;
                i += 1;
            }
        }
    }
    Ok(labels)
}

/// Go `PackDomainName`: appends `s` to `buf`, using and updating the compression map when given.
fn pack_name(s: &str, buf: &mut Vec<u8>, mut comp: Option<&mut HashMap<String, u16>>, compress: bool) -> Result<(), DnsError> {
    if s.is_empty() {
        return Ok(());
    }
    if !is_fqdn(s) {
        return Err(DnsError("domain must be fully qualified"));
    }
    if s == "." {
        buf.push(0);
        return Ok(());
    }
    let labels = name_labels(s)?;
    let mut pointer: Option<u16> = None;
    for (label, orig_start) in &labels {
        if label.is_empty() {
            return Err(DnsError("bad rdata"));
        }
        if let Some(map) = comp.as_deref_mut() {
            let key = &s[*orig_start..];
            if let Some(&p) = map.get(key) {
                if compress {
                    pointer = Some(p);
                    break;
                }
            } else if buf.len() < MAX_COMPRESSION_OFFSET {
                map.insert(key.to_string(), buf.len() as u16);
            }
        }
        buf.push(label.len() as u8);
        buf.extend_from_slice(label);
    }
    match pointer {
        Some(p) => buf.extend_from_slice(&(p ^ 0xC000).to_be_bytes()),
        None => buf.push(0),
    }
    Ok(())
}

/// Go `UnpackDomainName`: returns the name in presentation format and the offset after it.
pub fn unpack_name(msg: &[u8], mut off: usize) -> Result<(String, usize), DnsError> {
    let mut s = String::new();
    let mut off1 = 0usize;
    let mut budget = MAX_DOMAIN_NAME_WIRE_OCTETS;
    let mut pointers = 0usize;
    loop {
        if off >= msg.len() {
            return Err(DnsError("buffer size too small"));
        }
        let c = msg[off] as usize;
        off += 1;
        match c & 0xC0 {
            0x00 => {
                if c == 0 {
                    break;
                }
                if off + c > msg.len() {
                    return Err(DnsError("buffer size too small"));
                }
                budget -= c as i64 + 1;
                if budget <= 0 {
                    return Err(DnsError("domain name exceeded 255 wire-format octets"));
                }
                for &b in &msg[off..off + c] {
                    if matches!(b, b'.' | b' ' | b'\'' | b'@' | b';' | b'(' | b')' | b'"' | b'\\') {
                        s.push('\\');
                        s.push(b as char);
                    } else if !(b' '..=b'~').contains(&b) {
                        s.push_str(&format!("\\{b:03}"));
                    } else {
                        s.push(b as char);
                    }
                }
                s.push('.');
                off += c;
            }
            0xC0 => {
                if off >= msg.len() {
                    return Err(DnsError("buffer size too small"));
                }
                let c1 = msg[off] as usize;
                off += 1;
                if pointers == 0 {
                    off1 = off;
                }
                pointers += 1;
                if pointers > MAX_COMPRESSION_POINTERS {
                    return Err(DnsError("too many compression pointers"));
                }
                off = (c ^ 0xC0) << 8 | c1;
            }
            _ => return Err(DnsError("bad rdata")),
        }
    }
    if pointers == 0 {
        off1 = off;
    }
    if s.is_empty() {
        return Ok((".".to_string(), off1));
    }
    Ok((s, off1))
}

fn pack_rr(rr: &Rr, buf: &mut Vec<u8>, mut comp: Option<&mut HashMap<String, u16>>, compress: bool) -> Result<(), DnsError> {
    pack_name(&rr.name, buf, comp.as_deref_mut(), compress)?;
    buf.extend_from_slice(&rr.data.rtype().to_be_bytes());
    buf.extend_from_slice(&rr.class.to_be_bytes());
    buf.extend_from_slice(&rr.ttl.to_be_bytes());
    let len_pos = buf.len();
    buf.extend_from_slice(&[0, 0]);
    match &rr.data {
        RData::A(ip) => buf.extend_from_slice(&ip.octets()),
        RData::Aaaa(ip) => buf.extend_from_slice(&ip.octets()),
        RData::Ptr(name) => pack_name(name, buf, comp.as_deref_mut(), compress)?,
        RData::Srv { priority, weight, port, target } => {
            buf.extend_from_slice(&priority.to_be_bytes());
            buf.extend_from_slice(&weight.to_be_bytes());
            buf.extend_from_slice(&port.to_be_bytes());
            // miekg never compresses SRV targets (but records them for later names).
            pack_name(target, buf, comp.as_deref_mut(), false)?;
        }
        RData::Txt(strings) => {
            // Go quirk: an empty TXT list writes a zero byte without advancing, so rdata is empty.
            for s in strings {
                pack_txt_string(s, buf)?;
            }
        }
        RData::Other(_) => {}
    }
    let rdlen = buf.len() - len_pos - 2;
    let rdlen = u16::try_from(rdlen).map_err(|_| DnsError("rdata too large"))?;
    buf[len_pos..len_pos + 2].copy_from_slice(&rdlen.to_be_bytes());
    Ok(())
}

/// Go `packTxtString`: one length-prefixed string with `\X` and `\DDD` escapes decoded.
fn pack_txt_string(s: &str, buf: &mut Vec<u8>) -> Result<(), DnsError> {
    let bytes = s.as_bytes();
    let len_pos = buf.len();
    buf.push(0);
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 1;
            if i == bytes.len() {
                break;
            }
            if i + 2 < bytes.len() && bytes[i..i + 3].iter().all(u8::is_ascii_digit) {
                let value = u32::from(bytes[i] - b'0') * 100 + u32::from(bytes[i + 1] - b'0') * 10 + u32::from(bytes[i + 2] - b'0');
                buf.push(value as u8);
                i += 2;
            } else {
                buf.push(bytes[i]);
            }
        } else {
            buf.push(bytes[i]);
        }
        i += 1;
    }
    let len = buf.len() - len_pos - 1;
    if len > 255 {
        return Err(DnsError("string exceeded 255 bytes in txt"));
    }
    buf[len_pos] = len as u8;
    Ok(())
}

/// Go `unpackString` for TXT: bytes outside printable ASCII become `\DDD`, quotes and
/// backslashes are backslash-escaped.
fn unpack_txt_string(msg: &[u8], off: usize) -> Result<(String, usize), DnsError> {
    if off + 1 > msg.len() {
        return Err(DnsError("overflow unpacking txt"));
    }
    let l = msg[off] as usize;
    let off = off + 1;
    if off + l > msg.len() {
        return Err(DnsError("overflow unpacking txt"));
    }
    let mut s = String::with_capacity(l);
    for &b in &msg[off..off + l] {
        if b == b'"' || b == b'\\' {
            s.push('\\');
            s.push(b as char);
        } else if !(b' '..=b'~').contains(&b) {
            s.push_str(&format!("\\{b:03}"));
        } else {
            s.push(b as char);
        }
    }
    Ok((s, off + l))
}

fn unpack_rrs(count: u16, msg: &[u8], off: &mut usize) -> Result<Vec<Rr>, DnsError> {
    let mut out = Vec::new();
    for _ in 0..count {
        out.push(unpack_rr(msg, off)?);
    }
    Ok(out)
}

fn unpack_rr(msg: &[u8], off: &mut usize) -> Result<Rr, DnsError> {
    let (name, mut pos) = unpack_name(msg, *off)?;
    if pos + 10 > msg.len() {
        return Err(DnsError("overflow unpacking rr header"));
    }
    let rtype = u16::from_be_bytes([msg[pos], msg[pos + 1]]);
    let class = u16::from_be_bytes([msg[pos + 2], msg[pos + 3]]);
    let ttl = u32::from_be_bytes([msg[pos + 4], msg[pos + 5], msg[pos + 6], msg[pos + 7]]);
    let rdlen = u16::from_be_bytes([msg[pos + 8], msg[pos + 9]]) as usize;
    pos += 10;
    let end = pos + rdlen;
    if end > msg.len() {
        return Err(DnsError("bad rdlength"));
    }
    *off = end;
    let rr = |data| Rr { name: name.clone(), class, ttl, data };
    if rdlen == 0 {
        return Ok(rr(match rtype {
            TYPE_TXT => RData::Txt(Vec::new()),
            other => RData::Other(other),
        }));
    }
    // miekg decodes rdata against msg[:end], so name pointers cannot run past the record.
    let m = &msg[..end];
    let data = match rtype {
        TYPE_A => {
            if rdlen != 4 {
                return Err(DnsError("bad rdlength"));
            }
            RData::A(Ipv4Addr::new(m[pos], m[pos + 1], m[pos + 2], m[pos + 3]))
        }
        TYPE_AAAA => {
            if rdlen != 16 {
                return Err(DnsError("bad rdlength"));
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&m[pos..pos + 16]);
            RData::Aaaa(Ipv6Addr::from(octets))
        }
        TYPE_PTR => {
            let (ptr, next) = unpack_name(m, pos)?;
            if next != end {
                return Err(DnsError("bad rdlength"));
            }
            RData::Ptr(ptr)
        }
        TYPE_TXT => {
            let mut strings = Vec::new();
            let mut p = pos;
            while p < end {
                let (s, next) = unpack_txt_string(m, p)?;
                strings.push(s);
                p = next;
            }
            RData::Txt(strings)
        }
        TYPE_SRV => {
            let u16_at = |p: usize| -> Result<u16, DnsError> {
                if p + 2 > end { Err(DnsError("overflow unpacking uint16")) } else { Ok(u16::from_be_bytes([m[p], m[p + 1]])) }
            };
            let priority = u16_at(pos)?;
            let (mut weight, mut port, mut target) = (0, 0, String::new());
            let mut p = pos + 2;
            if p != end {
                weight = u16_at(p)?;
                p += 2;
                if p != end {
                    port = u16_at(p)?;
                    p += 2;
                    if p != end {
                        let (t, next) = unpack_name(m, p)?;
                        target = t;
                        p = next;
                    }
                }
            }
            if p != end {
                return Err(DnsError("bad rdlength"));
            }
            RData::Srv { priority, weight, port, target }
        }
        other => RData::Other(other),
    };
    Ok(rr(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Msg {
        let mut m = Msg { response: true, authoritative: true, compress: true, ..Msg::default() };
        m.answer.push(Rr {
            name: "_ai-gateway._tcp.local.".into(),
            class: CLASS_INET,
            ttl: 3200,
            data: RData::Ptr("CPA-8F3B._ai-gateway._tcp.local.".into()),
        });
        m.extra.push(Rr {
            name: "CPA-8F3B._ai-gateway._tcp.local.".into(),
            class: CLASS_INET | CLASS_TOP_BIT,
            ttl: 3200,
            data: RData::Srv { priority: 0, weight: 0, port: 8317, target: "host.local.".into() },
        });
        m.extra.push(Rr {
            name: "CPA-8F3B._ai-gateway._tcp.local.".into(),
            class: CLASS_INET,
            ttl: 3200,
            data: RData::Txt(vec!["version=1".into(), "tls=0".into()]),
        });
        m.extra.push(Rr { name: "host.local.".into(), class: CLASS_INET, ttl: 120, data: RData::A(Ipv4Addr::new(192, 0, 2, 10)) });
        m.extra.push(Rr {
            name: "host.local.".into(),
            class: CLASS_INET,
            ttl: 120,
            data: RData::Aaaa("2001:db8::1".parse().unwrap_or(Ipv6Addr::LOCALHOST)),
        });
        m
    }

    #[test]
    fn round_trips_compressed_message() {
        let packed = sample().pack().unwrap();
        let back = Msg::unpack(&packed).unwrap();
        assert!(back.response && back.authoritative);
        let mut expected = sample();
        expected.compress = false;
        assert_eq!(back, expected);
    }

    #[test]
    fn compression_shrinks_and_matches_miekg_layout() {
        let compressed = sample().pack().unwrap();
        let mut plain = sample();
        plain.compress = false;
        assert!(compressed.len() < plain.pack().unwrap().len());
        // The PTR target reuses the owner's "_ai-gateway._tcp.local." suffix: label + pointer.
        let needle = [8u8, b'C', b'P', b'A', b'-', b'8', b'F', b'3', b'B', 0xC0, 12];
        assert!(compressed.windows(needle.len()).any(|w| w == needle));
    }

    #[test]
    fn names_use_presentation_escapes() {
        // A space in an instance name is printed as "\ " and non-ASCII bytes as \DDD.
        let mut m = Msg { response: true, ..Msg::default() };
        m.answer.push(Rr { name: "My Node._x._tcp.local.".into(), class: 1, ttl: 1, data: RData::Ptr("a\\.b.local.".into()) });
        let back = Msg::unpack(&m.pack().unwrap()).unwrap();
        assert_eq!(back.answer[0].name, "My\\ Node._x._tcp.local.");
        assert_eq!(back.answer[0].data, RData::Ptr("a\\.b.local.".into()));
        let mut m = Msg::default();
        m.answer.push(Rr { name: "\u{7f3d}.local.".into(), class: 1, ttl: 1, data: RData::Other(99) });
        assert_eq!(Msg::unpack(&m.pack().unwrap()).unwrap().answer[0].name, "\\231\\188\\189.local.");
    }

    #[test]
    fn txt_strings_unescape_and_escape() {
        let mut m = Msg::default();
        m.answer.push(Rr { name: "x.".into(), class: 1, ttl: 1, data: RData::Txt(vec!["a=\\\"q\\\"".into(), "".into()]) });
        let back = Msg::unpack(&m.pack().unwrap()).unwrap();
        assert_eq!(back.answer[0].data, RData::Txt(vec!["a=\\\"q\\\"".into(), "".into()]));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Msg::unpack(&[0u8; 5]).is_err());
        let mut packed = sample().pack().unwrap();
        packed.truncate(packed.len() - 3);
        assert!(Msg::unpack(&packed).is_err());
        let mut m = Msg::default();
        m.question.push(Question { name: format!("{}.local.", "a".repeat(64)), qtype: 1, qclass: 1 });
        assert!(m.pack().is_err());
    }
}
