//! Digital signatures: checking the ones in a PDF and adding a new one.
//!
//! All cryptography is done by Windows (CryptoAPI). That is what makes
//! signature cards and USB tokens work: their certificates appear in the
//! Windows certificate store and Windows asks for the PIN. Trust is decided
//! by the Windows list of trusted root certificates.

use crate::edit;
use lopdf::{Dictionary, Document, IncrementalDocument, Object, ObjectId, Stream, StringFormat, dictionary};
use std::ffi::{CStr, c_void};
use std::ptr::{null, null_mut};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Security::Cryptography::UI::CryptUIDlgSelectCertificateFromStore;
use windows::Win32::Security::Cryptography::*;
use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows::core::{PSTR, w};

const ENC: u32 = X509_ASN_ENCODING.0 | PKCS_7_ASN_ENCODING.0;
/// Bytes kept free in the file for the signature itself.
const RESERVE: usize = 16384;
const OID_TIMESTAMP_TOKEN: &[u8] = b"1.2.840.113549.1.9.16.2.14";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Good,
    Warn,
    Bad,
}

#[derive(Clone, Debug, Default)]
pub struct SigInfo {
    /// A document timestamp rather than a person's signature.
    pub is_timestamp: bool,
    pub signer: String,
    pub org: String,
    pub issuer: String,
    /// Time stated by the signer's computer.
    pub signed_at: String,
    /// Time confirmed by a timestamp authority, when the signature carries one.
    pub timestamp: Option<String>,
    pub reason: String,
    pub location: String,
    pub cert_until: String,
    /// The signed bytes are unchanged and the signature matches them.
    pub intact: bool,
    pub intact_msg: String,
    /// The signature covers the file up to its last byte.
    pub whole: bool,
    pub trusted: bool,
    pub trust_msg: String,
}

impl SigInfo {
    pub fn level(&self) -> Level {
        if !self.intact {
            Level::Bad
        } else if !self.trusted {
            Level::Warn
        } else {
            Level::Good
        }
    }
}

/// One line that sums up all signatures of a document.
pub fn summary(sigs: &[SigInfo]) -> (Level, String) {
    let names: Vec<&str> = sigs.iter().filter(|s| !s.is_timestamp).map(|s| s.signer.as_str()).filter(|n| !n.is_empty()).collect();
    let who = if names.is_empty() { "Signed".to_string() } else { format!("Signed by {}", names.join(", ")) };
    let worst = sigs.iter().map(SigInfo::level).max().unwrap_or(Level::Good);
    let changed_after = sigs.last().is_some_and(|s| !s.whole);
    match worst {
        Level::Bad => (Level::Bad, format!("{who}. A signature is NOT valid: the document was changed or the signature is damaged.")),
        Level::Warn => (Level::Warn, format!("{who}. The document is unchanged, but the signer's identity could not be confirmed.")),
        Level::Good if changed_after => (Level::Warn, format!("{who}. The signed part is unchanged, but something was added to the file afterwards.")),
        Level::Good => (Level::Good, format!("{who}. The signature is valid and the document has not been changed.")),
    }
}

// ---------------------------------------------------------------------------
// Finding signatures in the raw file
// ---------------------------------------------------------------------------

struct RawSig {
    /// The file was rewritten after signing: the signed byte ranges no longer
    /// line up with the signature, so it cannot be valid any more.
    broken: bool,
    range: [usize; 4],
    contents: Vec<u8>,
    subfilter: String,
    m: String,
    reason: String,
    location: String,
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

fn rfind(hay: &[u8], needle: &[u8], before: usize) -> Option<usize> {
    hay[..before.min(hay.len())].windows(needle.len()).rposition(|w| w == needle)
}

fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\n' | b'\r' | b'\t' | 0x0c | 0)
}

pub fn has_signature_marker(bytes: &[u8]) -> bool {
    find(bytes, b"/ByteRange", 0).is_some()
}

fn scan(bytes: &[u8]) -> Vec<RawSig> {
    let mut out: Vec<RawSig> = Vec::new();
    let mut from = 0;
    while let Some(pos) = find(bytes, b"/ByteRange", from) {
        from = pos + 10;
        let Some(range) = parse_range(bytes, pos + 10) else { continue };
        let [a, b, c, d] = range;
        // What lies between the two signed parts must be exactly the signature string.
        let slot = (b > 0 && a + b < c && c + d <= bytes.len())
            .then(|| {
                let gap = &bytes[a + b..c];
                let start = gap.iter().position(|x| !is_ws(*x))?;
                let end = gap.iter().rposition(|x| !is_ws(*x))?;
                (gap[start] == b'<' && gap[end] == b'>' && end > start).then(|| unhex(&gap[start + 1..end]))
            })
            .flatten();
        let Some(contents) = slot else {
            // The ranges do not fit this file: it was rewritten after it was signed.
            let obj_start = rfind(bytes, b" obj", pos).unwrap_or(pos.saturating_sub(2000));
            let obj_end = find(bytes, b"endobj", pos).unwrap_or((pos + 80_000).min(bytes.len()));
            let dict = &bytes[obj_start.min(obj_end)..obj_end];
            let contents = hex_after(dict, b"/Contents");
            if contents.iter().all(|x| *x == 0) || out.iter().any(|s| s.contents == contents) {
                continue;
            }
            out.push(RawSig {
                broken: true,
                range: [0; 4],
                contents,
                subfilter: name_after(dict, b"/SubFilter"),
                m: string_after(dict, b"/M"),
                reason: string_after(dict, b"/Reason"),
                location: string_after(dict, b"/Location"),
            });
            continue;
        };
        if contents.iter().all(|x| *x == 0) || out.iter().any(|s| s.range == range) {
            continue;
        }
        // The signature dictionary: from its "obj" to its "endobj", without the signature string.
        let obj_start = rfind(bytes, b" obj", pos).unwrap_or(pos.saturating_sub(2000));
        let obj_end = find(bytes, b"endobj", pos.max(c)).unwrap_or((c + 2000).min(bytes.len()));
        let mut dict = Vec::new();
        for (s, e) in [(obj_start, a + b), (c, obj_end)] {
            if s < e && e <= bytes.len() {
                dict.extend_from_slice(&bytes[s..e]);
                dict.push(b' ');
            }
        }
        if obj_start >= a + b {
            dict = bytes[obj_start.min(obj_end)..obj_end].to_vec();
        }
        out.push(RawSig {
            broken: false,
            range,
            contents,
            subfilter: name_after(&dict, b"/SubFilter"),
            m: string_after(&dict, b"/M"),
            reason: string_after(&dict, b"/Reason"),
            location: string_after(&dict, b"/Location"),
        });
    }
    out.sort_by_key(|s| if s.broken { 0 } else { s.range[2] + s.range[3] });
    out
}

fn parse_range(bytes: &[u8], mut i: usize) -> Option<[usize; 4]> {
    let skip = |i: &mut usize| {
        while *i < bytes.len() && is_ws(bytes[*i]) {
            *i += 1;
        }
    };
    skip(&mut i);
    if *bytes.get(i)? != b'[' {
        return None;
    }
    i += 1;
    let mut out = [0usize; 4];
    for v in &mut out {
        skip(&mut i);
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start || i - start > 15 {
            return None;
        }
        *v = std::str::from_utf8(&bytes[start..i]).ok()?.parse().ok()?;
    }
    skip(&mut i);
    (*bytes.get(i)? == b']').then_some(out)
}

fn unhex(s: &[u8]) -> Vec<u8> {
    let digits: Vec<u8> = s.iter().filter_map(|c| (*c as char).to_digit(16).map(|d| d as u8)).collect();
    digits.chunks(2).map(|p| (p[0] << 4) | p.get(1).copied().unwrap_or(0)).collect()
}

/// Position right after `key` where the key is a whole name (not a prefix of a longer one).
fn after_key(dict: &[u8], key: &[u8], from: usize) -> Option<usize> {
    let mut from = from;
    while let Some(p) = find(dict, key, from) {
        let end = p + key.len();
        match dict.get(end) {
            Some(c) if is_ws(*c) || matches!(c, b'(' | b'<' | b'/' | b'[') => return Some(end),
            _ => from = end,
        }
    }
    None
}

/// The bytes of the hex string stored under `key`.
fn hex_after(dict: &[u8], key: &[u8]) -> Vec<u8> {
    let Some(mut i) = after_key(dict, key, 0) else { return Vec::new() };
    while i < dict.len() && is_ws(dict[i]) {
        i += 1;
    }
    if dict.get(i) != Some(&b'<') || dict.get(i + 1) == Some(&b'<') {
        return Vec::new();
    }
    let end = dict[i..].iter().position(|c| *c == b'>').map_or(dict.len(), |p| p + i);
    unhex(&dict[i + 1..end])
}

fn name_after(dict: &[u8], key: &[u8]) -> String {
    let Some(mut i) = after_key(dict, key, 0) else { return String::new() };
    while i < dict.len() && is_ws(dict[i]) {
        i += 1;
    }
    if dict.get(i) != Some(&b'/') {
        return String::new();
    }
    let end = dict[i + 1..].iter().position(|c| is_ws(*c) || matches!(c, b'/' | b'<' | b'>' | b'[' | b'(')).map_or(dict.len(), |p| p + i + 1);
    String::from_utf8_lossy(&dict[i + 1..end]).to_string()
}

/// The text string stored under `key`, or an empty string.
fn string_after(dict: &[u8], key: &[u8]) -> String {
    let mut from = 0;
    while let Some(mut i) = after_key(dict, key, from) {
        from = i;
        while i < dict.len() && is_ws(dict[i]) {
            i += 1;
        }
        let raw = match dict.get(i) {
            Some(b'(') => {
                let mut out = Vec::new();
                let mut depth = 1;
                i += 1;
                while i < dict.len() && depth > 0 {
                    let c = dict[i];
                    i += 1;
                    match c {
                        b'\\' => {
                            let Some(&e) = dict.get(i) else { break };
                            i += 1;
                            match e {
                                b'n' => out.push(b'\n'),
                                b'r' => out.push(b'\r'),
                                b't' => out.push(b'\t'),
                                b'0'..=b'7' => {
                                    let mut v = (e - b'0') as u32;
                                    for _ in 0..2 {
                                        match dict.get(i) {
                                            Some(o @ b'0'..=b'7') => {
                                                v = v * 8 + (o - b'0') as u32;
                                                i += 1;
                                            }
                                            _ => break,
                                        }
                                    }
                                    out.push(v as u8);
                                }
                                b'\n' | b'\r' => {}
                                other => out.push(other),
                            }
                        }
                        b'(' => {
                            depth += 1;
                            out.push(c);
                        }
                        b')' => {
                            depth -= 1;
                            if depth > 0 {
                                out.push(c);
                            }
                        }
                        _ => out.push(c),
                    }
                }
                out
            }
            Some(b'<') if dict.get(i + 1) != Some(&b'<') => {
                let end = dict[i..].iter().position(|c| *c == b'>').map_or(dict.len(), |p| p + i);
                unhex(&dict[i + 1..end])
            }
            _ => continue,
        };
        return decode_text(&raw);
    }
    String::new()
}

fn decode_text(raw: &[u8]) -> String {
    if raw.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = raw[2..].chunks(2).map(|p| u16::from_be_bytes([p[0], p.get(1).copied().unwrap_or(0)])).collect();
        String::from_utf16_lossy(&units)
    } else if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(&raw[3..]).to_string()
    } else {
        raw.iter().map(|b| *b as char).collect()
    }
}

/// Signatures are stored padded with zeros: cut the padding off.
fn trim_der(b: &[u8]) -> &[u8] {
    if b.len() >= 2 && b[0] == 0x30 {
        let l = b[1] as usize;
        let total = if l < 0x80 {
            Some(2 + l)
        } else if l == 0x80 {
            None
        } else {
            let n = l & 0x7f;
            (n <= 4 && b.len() >= 2 + n).then(|| 2 + n + b[2..2 + n].iter().fold(0usize, |acc, x| (acc << 8) | *x as usize))
        };
        if let Some(t) = total {
            if t <= b.len() {
                return &b[..t];
            }
        }
    }
    let end = b.iter().rposition(|x| *x != 0).map_or(0, |p| p + 1);
    &b[..end]
}

// ---------------------------------------------------------------------------
// Times
// ---------------------------------------------------------------------------

fn filetime_to_unix(ft: FILETIME) -> i64 {
    let t = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    (t / 10_000_000) as i64 - 11_644_473_600
}

fn unix_to_filetime(secs: i64) -> FILETIME {
    let t = ((secs + 11_644_473_600) as u64) * 10_000_000;
    FILETIME { dwLowDateTime: t as u32, dwHighDateTime: (t >> 32) as u32 }
}

fn show_time(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// Parse a PDF date such as `D:20260226143000+02'00'` into seconds since 1970.
fn parse_pdf_date(s: &str) -> Option<i64> {
    let s = s.trim().trim_start_matches("D:");
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() < 4 {
        return None;
    }
    let num = |from: usize, len: usize, default: u32| digits.get(from..from + len).and_then(|v| v.parse::<u32>().ok()).unwrap_or(default);
    let date = chrono::NaiveDate::from_ymd_opt(num(0, 4, 1970) as i32, num(4, 2, 1), num(6, 2, 1))?;
    let time = date.and_hms_opt(num(8, 2, 0), num(10, 2, 0), num(12, 2, 0))?;
    let rest = &s[digits.len()..];
    let mut offset = 0i64;
    if let Some(sign) = rest.chars().next().filter(|c| *c == '+' || *c == '-') {
        let tz: String = rest[1..].chars().filter(char::is_ascii_digit).collect();
        let hh = tz.get(0..2).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        let mm = tz.get(2..4).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        offset = (hh * 3600 + mm * 60) * if sign == '-' { -1 } else { 1 };
    }
    Some(time.and_utc().timestamp() - offset)
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// Check every signature in the file. Empty if the file is not signed.
pub fn verify_all(bytes: &[u8]) -> Vec<SigInfo> {
    scan(bytes).iter().map(|raw| unsafe { verify_one(bytes, raw) }).collect()
}

unsafe fn cert_text(cert: *const CERT_CONTEXT, kind: u32, flags: u32, oid: Option<windows::core::PCSTR>) -> String {
    let mut buf = [0u16; 512];
    let para = oid.map(|o| o.0 as *const c_void);
    let n = unsafe { CertGetNameStringW(cert, kind, flags, para, Some(&mut buf)) } as usize;
    String::from_utf16_lossy(&buf[..n.saturating_sub(1)])
}

unsafe fn fill_cert(info: &mut SigInfo, cert: *const CERT_CONTEXT) {
    unsafe {
        info.signer = cert_text(cert, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, None);
        info.org = cert_text(cert, CERT_NAME_ATTR_TYPE, 0, Some(szOID_ORGANIZATION_NAME));
        info.issuer = cert_text(cert, CERT_NAME_SIMPLE_DISPLAY_TYPE, CERT_NAME_ISSUER_FLAG, None);
        if !(*cert).pCertInfo.is_null() {
            info.cert_until = show_time(filetime_to_unix((*(*cert).pCertInfo).NotAfter));
        }
    }
}

/// Build the certificate chain and return Windows' error bits for it.
unsafe fn chain_errors(cert: *const CERT_CONTEXT, at: Option<FILETIME>, online: bool) -> Option<u32> {
    unsafe {
        let para = CERT_CHAIN_PARA { cbSize: size_of::<CERT_CHAIN_PARA>() as u32, dwUrlRetrievalTimeout: 8000, ..Default::default() };
        let mut chain: *mut CERT_CHAIN_CONTEXT = null_mut();
        let flags = if online { CERT_CHAIN_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT } else { 0 };
        let at_ptr = at.as_ref().map(|t| t as *const FILETIME);
        CertGetCertificateChain(None, cert, at_ptr, Some((*cert).hCertStore), &para, flags, None, &mut chain).ok()?;
        if chain.is_null() {
            return None;
        }
        let status = (*chain).TrustStatus.dwErrorStatus;
        CertFreeCertificateChain(chain);
        Some(status)
    }
}

unsafe fn judge_trust(info: &mut SigInfo, cert: *const CERT_CONTEXT, signed_at: Option<i64>, time_is_proven: bool) {
    const SOFT: u32 = CERT_TRUST_REVOCATION_STATUS_UNKNOWN | CERT_TRUST_IS_OFFLINE_REVOCATION;
    let Some(status) = (unsafe { chain_errors(cert, None, true) }) else {
        info.trust_msg = "Windows could not check the signer's certificate.".into();
        return;
    };
    let hard = status & !SOFT;
    let note = if status & SOFT != 0 { " Whether it has been revoked could not be checked." } else { "" };
    if hard == 0 {
        info.trusted = true;
        info.trust_msg = format!("The signer's certificate is trusted by Windows.{note}");
    } else if hard & CERT_TRUST_IS_REVOKED != 0 {
        info.trust_msg = "The signer's certificate has been revoked.".into();
    } else if hard & (CERT_TRUST_IS_UNTRUSTED_ROOT | CERT_TRUST_IS_PARTIAL_CHAIN) != 0 {
        info.trust_msg = "The issuer of the signer's certificate is not in the Windows list of trusted authorities.".into();
    } else if hard == CERT_TRUST_IS_NOT_TIME_VALID {
        // Expired today. What matters is whether it was valid when the document was signed.
        let then = signed_at.map(unix_to_filetime);
        let was_valid = then.is_some() && unsafe { chain_errors(cert, then, false) }.is_some_and(|s| s & !SOFT == 0);
        if was_valid {
            info.trusted = true;
            info.trust_msg = if time_is_proven {
                "The certificate has expired since, but it was valid at the timestamped signing time.".into()
            } else {
                "The certificate has expired since. It was valid at the signing time stated by the signer's computer.".into()
            };
        } else {
            info.trust_msg = "The signer's certificate is expired or not yet valid.".into();
        }
    } else {
        info.trust_msg = format!("The signer's certificate did not pass the Windows checks (code {hard:#x}).");
    }
}

unsafe fn msg_param(msg: *const c_void, param: u32) -> Option<(Vec<u64>, usize)> {
    unsafe {
        let mut cb = 0u32;
        CryptMsgGetParam(msg, param, 0, None, &mut cb).ok()?;
        // u64 storage keeps the returned structures properly aligned.
        let mut buf = vec![0u64; (cb as usize).div_ceil(8).max(1)];
        CryptMsgGetParam(msg, param, 0, Some(buf.as_mut_ptr().cast()), &mut cb).ok()?;
        Some((buf, cb as usize))
    }
}

/// The time of an embedded RFC 3161 timestamp, if there is one and it is genuine.
unsafe fn embedded_timestamp(pkcs7: &[u8]) -> Option<i64> {
    unsafe {
        let msg = CryptMsgOpenToDecode(ENC, CMSG_DETACHED_FLAG, 0, None, None, None);
        if msg.is_null() {
            return None;
        }
        let mut found = None;
        if CryptMsgUpdate(msg, Some(pkcs7), true).is_ok() {
            if let (Some((attrs, _)), Some((digest, digest_len))) = (msg_param(msg, CMSG_SIGNER_UNAUTH_ATTR_PARAM), msg_param(msg, CMSG_ENCRYPTED_DIGEST)) {
                let attrs = &*(attrs.as_ptr() as *const CRYPT_ATTRIBUTES);
                let digest = std::slice::from_raw_parts(digest.as_ptr() as *const u8, digest_len);
                for i in 0..attrs.cAttr as usize {
                    let attr = &*attrs.rgAttr.add(i);
                    if attr.pszObjId.is_null() || attr.cValue == 0 {
                        continue;
                    }
                    if CStr::from_ptr(attr.pszObjId.0.cast()).to_bytes() != OID_TIMESTAMP_TOKEN {
                        continue;
                    }
                    let value = &*attr.rgValue;
                    let token = std::slice::from_raw_parts(value.pbData, value.cbData as usize);
                    found = timestamp_time(token, digest).map(|(t, _)| t);
                }
            }
        }
        let _ = CryptMsgClose(Some(msg));
        found
    }
}

/// Verify a timestamp token over `data`. Returns its time and the authority's certificate.
unsafe fn timestamp_time(token: &[u8], data: &[u8]) -> Option<(i64, *mut CERT_CONTEXT)> {
    unsafe {
        let mut ctx: *mut CRYPT_TIMESTAMP_CONTEXT = null_mut();
        let mut signer: *mut CERT_CONTEXT = null_mut();
        CryptVerifyTimeStampSignature(token, Some(data), None, &mut ctx, &mut signer, None).ok()?;
        let mut time = None;
        if !ctx.is_null() {
            if !(*ctx).pTimeStamp.is_null() {
                time = Some(filetime_to_unix((*(*ctx).pTimeStamp).ftTime));
            }
            CryptMemFree(Some(ctx as *const c_void));
        }
        match time {
            Some(t) => Some((t, signer)),
            None => {
                if !signer.is_null() {
                    let _ = CertFreeCertificateContext(Some(signer));
                }
                None
            }
        }
    }
}

/// The signer's certificate taken from the signature itself, without verifying anything.
unsafe fn signer_cert(pkcs7: &[u8]) -> Option<*mut CERT_CONTEXT> {
    unsafe {
        let msg = CryptMsgOpenToDecode(ENC, CMSG_DETACHED_FLAG, 0, None, None, None);
        if msg.is_null() {
            return None;
        }
        let mut cert = None;
        if CryptMsgUpdate(msg, Some(pkcs7), true).is_ok() {
            if let Some((info, _)) = msg_param(msg, CMSG_SIGNER_CERT_INFO_PARAM) {
                if let Ok(store) = CertOpenStore(CERT_STORE_PROV_MSG, CERT_QUERY_ENCODING_TYPE(ENC), None, CERT_OPEN_STORE_FLAGS(0), Some(msg)) {
                    let c = CertGetSubjectCertificateFromStore(store, CERT_QUERY_ENCODING_TYPE(ENC), info.as_ptr() as *const CERT_INFO);
                    if !c.is_null() {
                        cert = Some(c);
                    }
                    let _ = CertCloseStore(Some(store), 0);
                }
            }
        }
        let _ = CryptMsgClose(Some(msg));
        cert
    }
}

unsafe fn verify_one(bytes: &[u8], raw: &RawSig) -> SigInfo {
    if raw.broken {
        let mut info = SigInfo {
            signed_at: parse_pdf_date(&raw.m).map(show_time).unwrap_or_default(),
            reason: raw.reason.clone(),
            location: raw.location.clone(),
            is_timestamp: raw.subfilter == "ETSI.RFC3161",
            intact_msg: "The document was changed after it was signed, so this signature is no longer valid.".into(),
            ..Default::default()
        };
        unsafe {
            if let Some(cert) = signer_cert(trim_der(&raw.contents)) {
                fill_cert(&mut info, cert);
                let _ = CertFreeCertificateContext(Some(cert));
            }
        }
        return info;
    }
    let [a, b, c, d] = raw.range;
    let (p1, p2) = (&bytes[a..a + b], &bytes[c..c + d]);
    let pkcs7 = trim_der(&raw.contents);
    let claimed = parse_pdf_date(&raw.m);
    let mut info = SigInfo {
        signed_at: claimed.map(show_time).unwrap_or_default(),
        reason: raw.reason.clone(),
        location: raw.location.clone(),
        whole: a == 0 && bytes[c + d..].iter().all(|x| is_ws(*x)),
        ..Default::default()
    };

    unsafe {
        match raw.subfilter.as_str() {
            "ETSI.RFC3161" => {
                info.is_timestamp = true;
                let data = [p1, p2].concat();
                match timestamp_time(pkcs7, &data) {
                    Some((time, cert)) => {
                        info.intact = true;
                        info.intact_msg = "The document has not been changed since this timestamp.".into();
                        info.timestamp = Some(show_time(time));
                        if !cert.is_null() {
                            fill_cert(&mut info, cert);
                            judge_trust(&mut info, cert, Some(time), true);
                            let _ = CertFreeCertificateContext(Some(cert));
                        }
                    }
                    None => info.intact_msg = "The timestamp does not match the document.".into(),
                }
            }
            "adbe.pkcs7.detached" | "ETSI.CAdES.detached" | "" => {
                if p1.len() > u32::MAX as usize || p2.len() > u32::MAX as usize {
                    info.intact_msg = "The file is too large to check.".into();
                    return info;
                }
                let para = CRYPT_VERIFY_MESSAGE_PARA {
                    cbSize: size_of::<CRYPT_VERIFY_MESSAGE_PARA>() as u32,
                    dwMsgAndCertEncodingType: ENC,
                    ..Default::default()
                };
                let parts = [p1.as_ptr(), p2.as_ptr()];
                let sizes = [p1.len() as u32, p2.len() as u32];
                let mut cert: *mut CERT_CONTEXT = null_mut();
                let res = CryptVerifyDetachedMessageSignature(&para, 0, pkcs7, 2, parts.as_ptr(), sizes.as_ptr(), Some(&mut cert));
                match res {
                    Ok(()) => {
                        info.intact = true;
                        info.intact_msg = if info.whole {
                            "The document has not been changed since it was signed.".into()
                        } else {
                            "The signed part has not been changed. More was added to the file after this signature (for example a later signature).".into()
                        };
                    }
                    Err(e) => {
                        info.intact_msg = match e.code().0 as u32 {
                            0x80091007 | 0x80090006 | 0xC000A000 =>"The document was changed after it was signed.".into(),
                            code => format!("The signature could not be checked (Windows code {code:#x})."),
                        };
                        cert = signer_cert(pkcs7).unwrap_or(null_mut());
                    }
                }
                let stamped = embedded_timestamp(pkcs7);
                info.timestamp = stamped.map(show_time);
                if !cert.is_null() {
                    fill_cert(&mut info, cert);
                    judge_trust(&mut info, cert, stamped.or(claimed), stamped.is_some());
                    let _ = CertFreeCertificateContext(Some(cert));
                } else if info.intact {
                    info.trust_msg = "The signer's certificate is missing from the signature.".into();
                }
            }
            other => info.intact_msg = format!("Signature type {other} is not supported."),
        }
    }
    info
}

// ---------------------------------------------------------------------------
// Certificates for signing
// ---------------------------------------------------------------------------

/// A certificate from the Windows store, with access to its private key.
pub struct Cert(*const CERT_CONTEXT);
// Certificate contexts are reference counted and may be used from any thread.
unsafe impl Send for Cert {}

impl Drop for Cert {
    fn drop(&mut self) {
        unsafe {
            let _ = CertFreeCertificateContext(Some(self.0));
        }
    }
}

impl Cert {
    pub fn name(&self) -> String {
        unsafe { cert_text(self.0, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, None) }
    }
}

/// A temporary store holding the user's certificates that can sign right now.
unsafe fn signing_store() -> Result<(HCERTSTORE, usize), String> {
    unsafe {
        let my = CertOpenSystemStoreW(None, w!("MY")).map_err(|e| format!("Cannot open the Windows certificate store: {}", e.message()))?;
        let mem = CertOpenStore(CERT_STORE_PROV_MEMORY, CERT_QUERY_ENCODING_TYPE(0), None, CERT_STORE_CREATE_NEW_FLAG, None)
            .map_err(|e| format!("Cannot list certificates: {}", e.message()))?;
        let mut count = 0;
        let mut cert: *mut CERT_CONTEXT = null_mut();
        loop {
            cert = CertEnumCertificatesInStore(my, (!cert.is_null()).then_some(cert as *const _));
            if cert.is_null() {
                break;
            }
            let mut cb = 0u32;
            let has_key = CertGetCertificateContextProperty(cert, CERT_KEY_PROV_INFO_PROP_ID, None, &mut cb).is_ok();
            let valid_now = CertVerifyTimeValidity(None, (*cert).pCertInfo) == 0;
            if has_key && valid_now && CertAddCertificateContextToStore(Some(mem), cert, CERT_STORE_ADD_ALWAYS, None).is_ok() {
                count += 1;
            }
        }
        let _ = CertCloseStore(Some(my), 0);
        Ok((mem, count))
    }
}

/// Let the user choose a signing certificate. `Ok(None)` if they cancelled.
pub fn pick_certificate() -> Result<Option<Cert>, String> {
    unsafe {
        let (store, count) = signing_store()?;
        if count == 0 {
            let _ = CertCloseStore(Some(store), 0);
            return Err("Windows has no certificate that can sign. Plug in your signature card or token and try again.".into());
        }
        let chosen = CryptUIDlgSelectCertificateFromStore(
            store,
            Some(GetActiveWindow()),
            w!("Sign document"),
            w!("Choose the certificate to sign with."),
            0,
            0,
            null(),
        );
        let _ = CertCloseStore(Some(store), 0);
        Ok((!chosen.is_null()).then(|| Cert(chosen)))
    }
}

/// Find a signing certificate by its SHA-1 thumbprint (hex, spaces allowed).
pub fn find_certificate(thumbprint: &str) -> Option<Cert> {
    let want = unhex(thumbprint.as_bytes());
    unsafe {
        let (store, _) = signing_store().ok()?;
        let mut found = None;
        let mut cert: *mut CERT_CONTEXT = null_mut();
        loop {
            cert = CertEnumCertificatesInStore(store, (!cert.is_null()).then_some(cert as *const _));
            if cert.is_null() {
                break;
            }
            let mut hash = [0u8; 20];
            let mut cb = 20u32;
            if CertGetCertificateContextProperty(cert, CERT_SHA1_HASH_PROP_ID, Some(hash.as_mut_ptr().cast()), &mut cb).is_ok() && hash[..] == want[..] {
                found = Some(Cert(CertDuplicateCertificateContext(Some(cert))));
            }
        }
        let _ = CertCloseStore(Some(store), 0);
        found
    }
}

// ---------------------------------------------------------------------------
// Signing
// ---------------------------------------------------------------------------

pub struct SignOptions {
    /// 0-based page that carries the visible signature.
    pub page: usize,
    /// Where the visible signature goes, in viewer coordinates (points from the
    /// top-left of the page as displayed). `None` signs without a visible mark.
    pub rect: Option<[f32; 4]>,
    pub reason: String,
    pub location: String,
}

fn text_string(s: &str) -> Object {
    if s.is_ascii() {
        Object::string_literal(s)
    } else {
        let mut b = vec![0xFE, 0xFF];
        b.extend(s.encode_utf16().flat_map(u16::to_be_bytes));
        Object::String(b, StringFormat::Hexadecimal)
    }
}

fn err<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> String {
    move |e| format!("{what}: {e}")
}

fn bulgarian_windows() -> bool {
    use windows::Win32::Globalization::GetUserDefaultUILanguage;
    unsafe { GetUserDefaultUILanguage() & 0x3ff == 0x02 }
}

/// Add `item` to the array stored under `key`, whether the array sits in the
/// dictionary itself or in an object of its own.
fn push_to_array(inc: &mut IncrementalDocument, dict: &mut Dictionary, key: &str, item: Object) -> Result<(), String> {
    match dict.get(key.as_bytes()).ok().cloned() {
        Some(Object::Reference(id)) => {
            inc.opt_clone_object_to_new_document(id).map_err(err("PDF structure"))?;
            match inc.new_document.get_object_mut(id) {
                Ok(Object::Array(a)) => a.push(item),
                _ => dict.set(key, vec![item]),
            }
        }
        Some(Object::Array(mut a)) => {
            a.push(item);
            dict.set(key, a);
        }
        _ => dict.set(key, vec![item]),
    }
    Ok(())
}

/// Sign the PDF. The signature is appended to the file, so signatures that
/// are already in it stay valid.
pub fn sign_pdf(bytes: &[u8], cert: &Cert, opts: &SignOptions) -> Result<Vec<u8>, String> {
    sign_inner(bytes, cert, opts, true)
}

fn sign_inner(bytes: &[u8], cert: &Cert, opts: &SignOptions, may_rewrite: bool) -> Result<Vec<u8>, String> {
    let doc = Document::load_mem(bytes).map_err(err("Cannot read the PDF"))?;
    if doc.encryption_state.is_some() || doc.trailer.has(b"Encrypt") {
        return Err("A password protected PDF cannot be signed. Save a copy without the password first.".into());
    }
    if doc.xref_start == 0 || doc.xref_recovery.is_some() {
        // The file's index is damaged, so nothing can be appended to it safely: write it out cleanly first.
        if !may_rewrite {
            return Err("This PDF is damaged and cannot be signed.".into());
        }
        let mut doc = doc;
        let mut clean = Vec::new();
        doc.save_to(&mut clean).map_err(err("Cannot rewrite the PDF"))?;
        return sign_inner(&clean, cert, opts, false);
    }

    let pages: Vec<ObjectId> = doc.page_iter().collect();
    let page_id = *pages.get(opts.page).ok_or("Page out of range")?;
    let root_id = doc.trailer.get(b"Root").and_then(Object::as_reference).map_err(err("PDF structure"))?;
    let (bbox, rot) = edit::page_box(&doc, page_id)?;
    let mut inc = IncrementalDocument::create_from(bytes.to_vec(), doc);

    let name = cert.name();
    let now = chrono::Local::now();
    let offset = now.offset().local_minus_utc();
    let pdf_date = format!("D:{}{}{:02}'{:02}'", now.format("%Y%m%d%H%M%S"), if offset < 0 { '-' } else { '+' }, offset.abs() / 3600, offset.abs() % 3600 / 60);

    // The signature dictionary, with room left for the values filled in below.
    let mut sig = Dictionary::new();
    sig.set("Type", "Sig");
    sig.set("Filter", "Adobe.PPKLite");
    sig.set("SubFilter", "adbe.pkcs7.detached");
    sig.set("ByteRange", vec![Object::Integer(0), Object::Integer(9_999_999_999), Object::Integer(9_999_999_999), Object::Integer(9_999_999_999)]);
    sig.set("Contents", Object::String(vec![0u8; RESERVE], StringFormat::Hexadecimal));
    sig.set("M", Object::string_literal(pdf_date));
    sig.set("Name", text_string(&name));
    if !opts.reason.trim().is_empty() {
        sig.set("Reason", text_string(opts.reason.trim()));
    }
    if !opts.location.trim().is_empty() {
        sig.set("Location", text_string(opts.location.trim()));
    }
    let sig_id = inc.new_document.add_object(sig);

    // The form field that shows the signature on the page (or an invisible one).
    let mut widget = dictionary! {
        "Type" => "Annot",
        "Subtype" => "Widget",
        "FT" => "Sig",
        "T" => Object::string_literal(format!("Signature{}", sig_id.0)),
        "V" => sig_id,
        "F" => 132,
        "P" => page_id,
    };
    match opts.rect {
        Some([x0, y0, x1, y1]) => {
            let (w, h) = ((x1 - x0).abs().max(20.0), (y1 - y0).abs().max(12.0));
            let ((ax, ay), m) = edit::view_to_user(bbox, rot, x0.min(x1), y0.min(y1));
            let ((bx, by), _) = edit::view_to_user(bbox, rot, x0.min(x1) + w, y0.min(y1) + h);
            widget.set("Rect", vec![Object::Real(ax.min(bx)), Object::Real(ay.min(by)), Object::Real(ax.max(bx)), Object::Real(ay.max(by))]);

            let label = if bulgarian_windows() { "Електронно подписано от" } else { "Digitally signed by" };
            let date = now.format("%Y-%m-%d %H:%M %:z").to_string();
            let reason = opts.reason.trim();
            let font = edit::embed_font(&mut inc.new_document, &format!("{label}{name}{date}{reason}"))?;
            let pad = 5.0;
            let mut name_size = (h * 0.22).clamp(6.0, 11.0);
            while name_size > 4.5 && font.width(&name, name_size) > w - 2.0 * pad {
                name_size -= 0.5;
            }
            let small = (h * 0.14).clamp(4.5, 7.0);
            let mut ops = format!("q\n0.13 0.38 0.72 RG 0.8 w 0.4 0.4 {:.2} {:.2} re S\nBT\n", w - 0.8, h - 0.8);
            let mut line = |text: &str, size: f32, y: f32, gray: f32| {
                if !text.is_empty() {
                    ops.push_str(&format!("/F1 {size:.2} Tf {gray} g 1 0 0 1 {pad} {y:.2} Tm <{}> Tj\n", font.hex(text)));
                }
            };
            line(label, small, h - pad - small, 0.35);
            line(&name, name_size, h - pad - small - name_size - 2.0, 0.0);
            line(&date, small, h - pad - small - name_size - small - 5.0, 0.35);
            line(reason, small, h - pad - small - name_size - 2.0 * small - 7.0, 0.35);
            ops.push_str("ET\nQ\n");
            let ap = Stream::new(
                dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Form",
                    "FormType" => 1,
                    "BBox" => vec![Object::Real(0.0), Object::Real(0.0), Object::Real(w), Object::Real(h)],
                    "Matrix" => vec![Object::Real(m[0]), Object::Real(m[1]), Object::Real(m[2]), Object::Real(m[3]), Object::Real(0.0), Object::Real(0.0)],
                    "Resources" => dictionary! { "Font" => dictionary! { "F1" => font.id } },
                },
                ops.into_bytes(),
            );
            let ap_id = inc.new_document.add_object(ap);
            widget.set("AP", dictionary! { "N" => ap_id });
        }
        None => widget.set("Rect", vec![Object::Integer(0); 4]),
    }
    let widget_id = inc.new_document.add_object(widget);

    // Hook the field into the page and into the document's form.
    inc.opt_clone_object_to_new_document(page_id).map_err(err("PDF structure"))?;
    let mut page = inc.new_document.get_dictionary(page_id).map_err(err("Page"))?.clone();
    push_to_array(&mut inc, &mut page, "Annots", Object::Reference(widget_id))?;
    inc.new_document.set_object(page_id, page);

    inc.opt_clone_object_to_new_document(root_id).map_err(err("PDF structure"))?;
    let mut root = inc.new_document.get_dictionary(root_id).map_err(err("PDF structure"))?.clone();
    match root.get(b"AcroForm").ok().cloned() {
        Some(Object::Reference(form_id)) => {
            inc.opt_clone_object_to_new_document(form_id).map_err(err("PDF structure"))?;
            let mut form = inc.new_document.get_dictionary(form_id).map_err(err("PDF form"))?.clone();
            push_to_array(&mut inc, &mut form, "Fields", Object::Reference(widget_id))?;
            form.set("SigFlags", 3);
            inc.new_document.set_object(form_id, form);
        }
        other => {
            let mut form = match other {
                Some(Object::Dictionary(d)) => d,
                _ => Dictionary::new(),
            };
            push_to_array(&mut inc, &mut form, "Fields", Object::Reference(widget_id))?;
            form.set("SigFlags", 3);
            root.set("AcroForm", form);
        }
    }
    inc.new_document.set_object(root_id, root);

    let mut out = Vec::with_capacity(bytes.len() + RESERVE * 2 + 8192);
    inc.save_to(&mut out).map_err(err("Cannot write the PDF"))?;

    // Now that every byte has its place, record which bytes are signed.
    let tail = bytes.len().min(out.len());
    let zeros = vec![b'0'; RESERVE * 2];
    let lt = find(&out, &zeros, tail).and_then(|p| p.checked_sub(1)).ok_or("Internal error: signature slot not found")?;
    let gt = lt + 1 + RESERVE * 2;
    if out.get(lt) != Some(&b'<') || out.get(gt) != Some(&b'>') {
        return Err("Internal error: signature slot is malformed".into());
    }
    let key = find(&out, b"/ByteRange", tail).ok_or("Internal error: byte range not found")?;
    let open = find(&out, b"[", key).ok_or("Internal error: byte range not found")?;
    let close = find(&out, b"]", open).ok_or("Internal error: byte range not found")?;
    let range = format!("0 {} {} {}", lt, gt + 1, out.len() - (gt + 1));
    if range.len() > close - open - 1 {
        return Err("Internal error: byte range does not fit".into());
    }
    let padded = format!("{range:<width$}", width = close - open - 1);
    out[open + 1..close].copy_from_slice(padded.as_bytes());

    let signature = unsafe { cms_sign(cert, &out[..lt], &out[gt + 1..])? };
    if signature.len() > RESERVE {
        return Err("The signature is larger than expected and does not fit.".into());
    }
    for (i, b) in signature.iter().enumerate() {
        let hex = format!("{b:02X}");
        out[lt + 1 + 2 * i..lt + 3 + 2 * i].copy_from_slice(hex.as_bytes());
    }

    // Never hand back a file whose new signature does not check out.
    match verify_all(&out).last() {
        Some(s) if s.intact && s.whole => Ok(out),
        _ => Err("The signed file did not pass its own check, so it was not saved.".into()),
    }
}

/// Create the PKCS#7 signature over the two signed parts of the file.
unsafe fn cms_sign(cert: &Cert, p1: &[u8], p2: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        if p1.len() > u32::MAX as usize || p2.len() > u32::MAX as usize {
            return Err("The file is too large to sign.".into());
        }
        // Include the issuing certificates so that others can check the signature.
        let mut certs: Vec<*mut CERT_CONTEXT> = Vec::new();
        let para = CERT_CHAIN_PARA { cbSize: size_of::<CERT_CHAIN_PARA>() as u32, ..Default::default() };
        let mut chain: *mut CERT_CHAIN_CONTEXT = null_mut();
        if CertGetCertificateChain(None, cert.0, None, None, &para, 0, None, &mut chain).is_ok() && !chain.is_null() {
            if (*chain).cChain > 0 {
                let simple = *(*chain).rgpChain;
                let n = (*simple).cElement as usize;
                for i in 0..n {
                    // The root is left out: verifiers must have it themselves.
                    if n > 1 && i == n - 1 {
                        break;
                    }
                    let element = *(*simple).rgpElement.add(i);
                    certs.push((*element).pCertContext as *mut CERT_CONTEXT);
                }
            }
        }
        if certs.is_empty() {
            certs.push(cert.0 as *mut CERT_CONTEXT);
        }

        let para = CRYPT_SIGN_MESSAGE_PARA {
            cbSize: size_of::<CRYPT_SIGN_MESSAGE_PARA>() as u32,
            dwMsgEncodingType: ENC,
            pSigningCert: cert.0,
            HashAlgorithm: CRYPT_ALGORITHM_IDENTIFIER { pszObjId: PSTR(szOID_NIST_sha256.0 as *mut u8), ..Default::default() },
            cMsgCert: certs.len() as u32,
            rgpMsgCert: certs.as_mut_ptr(),
            ..Default::default()
        };
        let parts = [p1.as_ptr(), p2.as_ptr()];
        let sizes = [p1.len() as u32, p2.len() as u32];
        let mut blob = vec![0u8; RESERVE];
        let mut cb = blob.len() as u32;
        let res = CryptSignMessage(&para, true, 2, Some(parts.as_ptr()), sizes.as_ptr(), Some(blob.as_mut_ptr()), &mut cb);
        if !chain.is_null() {
            CertFreeCertificateChain(chain);
        }
        match res {
            Ok(()) => {
                blob.truncate(cb as usize);
                Ok(blob)
            }
            Err(e) => Err(match e.code().0 as u32 {
                0x8010006E | 0x800704C7 | 0x80090036 => "Signing was cancelled.".to_string(),
                0x8010006B | 0x8010006C => "The PIN was wrong or the card is blocked.".to_string(),
                _ => format!("Windows could not sign with this certificate: {}", e.message()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_dates() {
        // 14:30 at +02:00 is 12:30 UTC.
        assert_eq!(parse_pdf_date("D:20260226143000+02'00'"), Some(1772109000));
        assert_eq!(parse_pdf_date("D:20260226123000Z"), Some(1772109000));
        assert_eq!(parse_pdf_date("D:2026"), Some(1767225600));
        assert_eq!(parse_pdf_date("yesterday"), None);
    }

    #[test]
    fn signature_padding_is_cut() {
        let mut der = vec![0x30, 0x82, 0x01, 0x00];
        der.extend(vec![7u8; 256]);
        let mut padded = der.clone();
        padded.extend(vec![0u8; 500]);
        assert_eq!(trim_der(&padded), &der[..]);
        assert_eq!(trim_der(&[0x30, 0x03, 1, 2, 3, 0, 0]), &[0x30, 0x03, 1, 2, 3]);
    }

    #[test]
    fn dictionary_values() {
        let dict = b"<< /Type /Sig /SubFilter /adbe.pkcs7.detached /Prop_Build << /App << /Name /Acrobat >> >> \
                     /Name (Ivan \\(test\\) Petrov) /M (D:20260101120000Z) /MediaBoxLike 1 \
                     /Reason <FEFF041F0440043E04310430> >>";
        assert_eq!(name_after(dict, b"/SubFilter"), "adbe.pkcs7.detached");
        assert_eq!(string_after(dict, b"/Name"), "Ivan (test) Petrov");
        assert_eq!(string_after(dict, b"/M"), "D:20260101120000Z");
        assert_eq!(string_after(dict, b"/Reason"), "Проба");
        assert_eq!(string_after(dict, b"/Location"), "");
    }

    #[test]
    fn unsigned_bytes_have_no_signatures() {
        assert!(verify_all(b"%PDF-1.4\n1 0 obj << /ByteRange [0 10 20 5] >> endobj\n%%EOF").is_empty());
        assert!(verify_all(b"%PDF-1.4 nothing here").is_empty());
    }
}
