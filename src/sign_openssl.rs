//! Signature cryptography on Linux, through OpenSSL.
//!
//! Checking: the signature is verified with OpenSSL's CMS code, which handles
//! RSA, RSA-PSS and ECDSA signatures in both the adbe.pkcs7.detached and the
//! ETSI.CAdES.detached form, and RFC 3161 timestamps. The signer is trusted
//! when the certificate chain leads to an authority in the system's list
//! (`/etc/ssl/certs`, the `ca-certificates` package) or to a root certificate
//! the user put in `~/.local/share/ProtoPDF/trusted`. Revocation is not checked.
//!
//! Signing: with a certificate file (.p12 / .pfx) that holds the certificate
//! and its private key. Smart cards and USB tokens are not supported here.

use super::*;
use openssl::asn1::{Asn1Object, Asn1Time, Asn1TimeRef};
use openssl::cms::{CMSOptions, CmsContentInfo};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkcs12::Pkcs12;
use openssl::pkey::{PKey, Private};
use openssl::stack::Stack;
use openssl::x509::store::X509StoreBuilder;
use openssl::x509::verify::X509VerifyParam;
use openssl::x509::{X509, X509NameRef, X509Ref, X509StoreContext, X509VerifyResult};
use std::path::{Path, PathBuf};

const OID_TIMESTAMP_TOKEN: &str = "1.2.840.113549.1.9.16.2.14";

/// Folder for root certificates the user trusts in addition to the system's
/// list, for example the root of a national qualified signature provider.
pub fn trusted_dir() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    data.join("ProtoPDF").join("trusted")
}

pub(super) fn ui_is_bulgarian() -> bool {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .is_some_and(|l| l.starts_with("bg"))
}

// ---------------------------------------------------------------------------
// Reading the signature's structure (DER, or BER as some signers write it)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Tlv<'a> {
    tag: u8,
    body: &'a [u8],
    /// The whole element, header included.
    whole: &'a [u8],
}

/// The element at the start of `b`, and what follows it.
fn tlv(b: &[u8]) -> Option<(Tlv<'_>, &[u8])> {
    tlv_at(b, 0)
}

fn tlv_at(b: &[u8], depth: u32) -> Option<(Tlv<'_>, &[u8])> {
    let tag = *b.first()?;
    // Multi-byte tags do not occur in signatures.
    if tag & 0x1f == 0x1f || depth > 40 {
        return None;
    }
    let l0 = *b.get(1)?;
    let (start, len) = if l0 < 0x80 {
        (2, l0 as usize)
    } else if l0 == 0x80 {
        // Indefinite length: the contents run up to an end-of-contents marker.
        if tag & 0x20 == 0 {
            return None;
        }
        let mut rest = &b[2..];
        let mut used = 0;
        while !rest.starts_with(&[0, 0]) {
            let (el, after) = tlv_at(rest, depth + 1)?;
            used += el.whole.len();
            rest = after;
        }
        let end = 2 + used + 2;
        return Some((Tlv { tag, body: &b[2..2 + used], whole: &b[..end] }, &b[end..]));
    } else {
        let n = (l0 & 0x7f) as usize;
        if n > 4 || b.len() < 2 + n {
            return None;
        }
        (2 + n, b[2..2 + n].iter().fold(0usize, |acc, x| (acc << 8) | *x as usize))
    };
    let end = start.checked_add(len)?;
    if end > b.len() {
        return None;
    }
    Some((Tlv { tag, body: &b[start..end], whole: &b[..end] }, &b[end..]))
}

fn children(body: &[u8]) -> Vec<Tlv<'_>> {
    let mut out = Vec::new();
    let mut rest = body;
    while !rest.is_empty() && !rest.starts_with(&[0, 0]) {
        let Some((el, after)) = tlv(rest) else { break };
        out.push(el);
        rest = after;
    }
    out
}

fn oid_string(body: &[u8]) -> String {
    let mut parts: Vec<u64> = Vec::new();
    let mut v: u64 = 0;
    for &x in body {
        v = (v << 7) | (x & 0x7f) as u64;
        if x & 0x80 == 0 {
            if parts.is_empty() {
                let first = (v / 40).min(2);
                parts.push(first);
                parts.push(v - first * 40);
            } else {
                parts.push(v);
            }
            v = 0;
        }
    }
    parts.iter().map(u64::to_string).collect::<Vec<_>>().join(".")
}

/// What is needed from a CMS / PKCS#7 SignedData structure.
struct SignedData<'a> {
    certs: Vec<X509>,
    /// How the signer names its certificate.
    sid: Option<Tlv<'a>>,
    /// The signature value.
    signature: &'a [u8],
    /// Unsigned attributes of the signer: type and values.
    unsigned: Vec<(String, Vec<Tlv<'a>>)>,
}

fn parse_signed_data(der: &[u8]) -> Option<SignedData<'_>> {
    let (content_info, _) = tlv(der)?;
    let ci = children(content_info.body);
    let explicit = ci.get(1).filter(|t| t.tag == 0xA0)?;
    let (sd, _) = tlv(explicit.body)?;
    // version, digestAlgorithms, encapContentInfo, [0] certificates, [1] crls, signerInfos
    let parts = children(sd.body);
    let certs = parts
        .iter()
        .skip(3)
        .find(|t| t.tag == 0xA0)
        .map(|t| children(t.body).iter().filter(|c| c.tag == 0x30).filter_map(|c| X509::from_der(c.whole).ok()).collect())
        .unwrap_or_default();
    let infos = parts.last().filter(|t| t.tag == 0x31 && parts.len() > 3)?;
    let info = children(infos.body).into_iter().next()?;
    // version, sid, digestAlgorithm, [0] signedAttrs, signatureAlgorithm, signature, [1] unsignedAttrs
    let fields = children(info.body);
    let signature = fields.iter().skip(2).find(|t| t.tag == 0x04)?.body;
    let unsigned = fields
        .iter()
        .skip(2)
        .find(|t| t.tag == 0xA1)
        .map(|t| {
            children(t.body)
                .iter()
                .filter_map(|attr| {
                    let a = children(attr.body);
                    let oid = a.first().filter(|o| o.tag == 0x06)?;
                    let values = a.get(1).filter(|s| s.tag == 0x31)?;
                    Some((oid_string(oid.body), children(values.body)))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(SignedData { certs, sid: fields.get(1).copied(), signature, unsigned })
}

fn strip_zeros(b: &[u8]) -> &[u8] {
    let i = b.iter().position(|x| *x != 0).unwrap_or(b.len());
    &b[i..]
}

/// The signer's certificate, picked out of the certificates in the signature.
fn signer_of(sd: &SignedData) -> Option<X509> {
    let matches = |cert: &X509| -> bool {
        let Some(sid) = sd.sid else { return false };
        match sid.tag {
            // issuerAndSerialNumber
            0x30 => {
                let f = children(sid.body);
                let (Some(issuer), Some(serial)) = (f.first(), f.get(1)) else { return false };
                let same_serial = cert.serial_number().to_bn().is_ok_and(|bn| bn.to_vec() == strip_zeros(serial.body));
                let same_issuer = cert.issuer_name().to_der().is_ok_and(|d| d == issuer.whole);
                same_serial && same_issuer
            }
            // [0] subjectKeyIdentifier
            0x80 => cert.subject_key_id().is_some_and(|k| k.as_slice() == sid.body),
            _ => false,
        }
    };
    sd.certs.iter().find(|c| matches(c)).or_else(|| (sd.certs.len() == 1).then(|| &sd.certs[0])).cloned()
}

// ---------------------------------------------------------------------------
// Certificates
// ---------------------------------------------------------------------------

fn unix_time(t: &Asn1TimeRef) -> Option<i64> {
    let d = Asn1Time::from_unix(0).ok()?.diff(t).ok()?;
    Some(d.days as i64 * 86400 + d.secs as i64)
}

fn name_entry(name: &X509NameRef, nid: Nid) -> String {
    name.entries_by_nid(nid).next().and_then(|e| e.data().to_string().ok()).unwrap_or_default()
}

/// The name a person would use for a certificate subject or issuer.
fn display_name(name: &X509NameRef) -> String {
    [Nid::COMMONNAME, Nid::ORGANIZATIONALUNITNAME, Nid::ORGANIZATIONNAME, Nid::PKCS9_EMAILADDRESS]
        .into_iter()
        .map(|nid| name_entry(name, nid))
        .find(|s| !s.is_empty())
        .unwrap_or_default()
}

fn fill_cert(info: &mut SigInfo, cert: &X509Ref) {
    info.signer = display_name(cert.subject_name());
    info.org = name_entry(cert.subject_name(), Nid::ORGANIZATIONNAME);
    info.issuer = display_name(cert.issuer_name());
    info.cert_until = unix_time(cert.not_after()).map(show_time).unwrap_or_default();
}

/// Root certificates the user added to `trusted_dir`, in PEM or DER files.
fn user_roots() -> Vec<X509> {
    let Ok(dir) = std::fs::read_dir(trusted_dir()) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in dir.flatten() {
        let Ok(bytes) = std::fs::read(entry.path()) else { continue };
        if bytes.windows(10).any(|w| w == b"-----BEGIN") {
            out.extend(X509::stack_from_pem(&bytes).unwrap_or_default());
        } else if let Ok(c) = X509::from_der(&bytes) {
            out.push(c);
        }
    }
    out
}

/// Build the certificate chain, at `at` (seconds since 1970) or now.
fn check_chain(cert: &X509Ref, extra: &[X509], at: Option<i64>) -> Option<X509VerifyResult> {
    let mut builder = X509StoreBuilder::new().ok()?;
    let _ = builder.set_default_paths();
    for root in user_roots() {
        let _ = builder.add_cert(root);
    }
    if let Some(t) = at {
        let mut param = X509VerifyParam::new().ok()?;
        param.set_time(t as _);
        builder.set_param(&param).ok()?;
    }
    let store = builder.build();
    let mut chain = Stack::new().ok()?;
    for c in extra {
        chain.push(c.clone()).ok()?;
    }
    let mut ctx = X509StoreContext::new().ok()?;
    ctx.init(&store, cert, &chain, |c| {
        c.verify_cert()?;
        Ok(c.error())
    })
    .ok()
}

fn judge_trust(info: &mut SigInfo, cert: &X509Ref, extra: &[X509], signed_at: Option<i64>, time_is_proven: bool) {
    // OpenSSL's X509_V_ERR_* codes.
    const OK: i32 = 0;
    const UNABLE_TO_GET_ISSUER_CERT: i32 = 2;
    const CERT_NOT_YET_VALID: i32 = 9;
    const CERT_HAS_EXPIRED: i32 = 10;
    const DEPTH_ZERO_SELF_SIGNED_CERT: i32 = 18;
    const SELF_SIGNED_CERT_IN_CHAIN: i32 = 19;
    const UNABLE_TO_GET_ISSUER_CERT_LOCALLY: i32 = 20;
    const UNABLE_TO_VERIFY_LEAF_SIGNATURE: i32 = 21;
    const CERT_REVOKED: i32 = 23;

    let Some(result) = check_chain(cert, extra, None) else {
        info.trust_msg = "The signer's certificate could not be checked.".into();
        return;
    };
    match result.as_raw() {
        OK => {
            info.trusted = true;
            info.trust_msg = "The signer's certificate is trusted by this computer. Whether it has been revoked was not checked.".into();
        }
        CERT_REVOKED => info.trust_msg = "The signer's certificate has been revoked.".into(),
        UNABLE_TO_GET_ISSUER_CERT | DEPTH_ZERO_SELF_SIGNED_CERT | SELF_SIGNED_CERT_IN_CHAIN | UNABLE_TO_GET_ISSUER_CERT_LOCALLY
        | UNABLE_TO_VERIFY_LEAF_SIGNATURE => {
            info.trust_msg = format!(
                "The issuer of the signer's certificate is not in this computer's list of trusted authorities. \
                 To trust it, copy its root certificate into {}",
                trusted_dir().display()
            );
        }
        CERT_HAS_EXPIRED | CERT_NOT_YET_VALID => {
            // Expired today. What matters is whether it was valid when the document was signed.
            let was_valid = signed_at.and_then(|t| check_chain(cert, extra, Some(t))).is_some_and(|r| r.as_raw() == OK);
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
        }
        _ => info.trust_msg = format!("The signer's certificate did not pass the checks ({}).", result.error_string()),
    }
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

struct Stamp {
    time: i64,
    cert: Option<X509>,
    certs: Vec<X509>,
}

/// Check an RFC 3161 timestamp token over `data`.
fn timestamp(token: &[u8], data: &[u8]) -> Option<Stamp> {
    let mut cms = CmsContentInfo::from_der(token).ok()?;
    let mut tst = Vec::new();
    cms.verify(None, None, None, Some(&mut tst), CMSOptions::NO_SIGNER_CERT_VERIFY | CMSOptions::BINARY).ok()?;
    // TSTInfo: version, policy, messageImprint { hashAlgorithm, hashedMessage }, serialNumber, genTime, ...
    let (info, _) = tlv(&tst)?;
    let fields = children(info.body);
    let imprint = children(fields.get(2)?.body);
    let alg = children(imprint.first()?.body);
    let oid = Asn1Object::from_str(&oid_string(alg.first()?.body)).ok()?;
    let md = MessageDigest::from_nid(oid.nid())?;
    if openssl::hash::hash(md, data).ok()?.as_ref() != imprint.get(1)?.body {
        return None;
    }
    let gen = std::str::from_utf8(fields.iter().find(|t| t.tag == 0x18)?.body).ok()?;
    let time = chrono::NaiveDateTime::parse_from_str(gen.get(..14)?, "%Y%m%d%H%M%S").ok()?.and_utc().timestamp();
    let sd = parse_signed_data(token)?;
    Some(Stamp { time, cert: signer_of(&sd), certs: sd.certs })
}

/// The time of an embedded RFC 3161 timestamp, if there is one and it is genuine.
fn embedded_timestamp(sd: &SignedData) -> Option<i64> {
    let (_, values) = sd.unsigned.iter().find(|(oid, _)| oid == OID_TIMESTAMP_TOKEN)?;
    timestamp(values.first()?.whole, sd.signature).map(|s| s.time)
}

fn describe_failure(e: &openssl::error::ErrorStack) -> String {
    let reasons: Vec<String> = e.errors().iter().filter_map(|x| x.reason().map(str::to_string)).collect();
    let all = reasons.join("; ");
    if all.contains("certificate not found") {
        "The signer's certificate is missing from the signature, so it cannot be checked.".into()
    } else if all.contains("unsupported") || all.contains("unknown") {
        format!("The signature uses a method this computer cannot check ({all}).")
    } else {
        "The document was changed after it was signed.".into()
    }
}

pub(super) fn verify(bytes: &[u8], raw: &RawSig) -> SigInfo {
    let pkcs7 = trim_der(&raw.contents);
    let parsed = parse_signed_data(pkcs7);
    let claimed = parse_pdf_date(&raw.m);
    if raw.broken {
        let mut info = SigInfo {
            signed_at: claimed.map(show_time).unwrap_or_default(),
            reason: raw.reason.clone(),
            location: raw.location.clone(),
            is_timestamp: raw.subfilter == "ETSI.RFC3161",
            intact_msg: "The document was changed after it was signed, so this signature is no longer valid.".into(),
            ..Default::default()
        };
        if let Some(cert) = parsed.as_ref().and_then(signer_of) {
            fill_cert(&mut info, &cert);
        }
        return info;
    }
    let [a, b, c, d] = raw.range;
    let (p1, p2) = (&bytes[a..a + b], &bytes[c..c + d]);
    let mut info = SigInfo {
        signed_at: claimed.map(show_time).unwrap_or_default(),
        reason: raw.reason.clone(),
        location: raw.location.clone(),
        whole: a == 0 && bytes[c + d..].iter().all(|x| is_ws(*x)),
        ..Default::default()
    };
    let data = [p1, p2].concat();

    match raw.subfilter.as_str() {
        "ETSI.RFC3161" => {
            info.is_timestamp = true;
            match timestamp(pkcs7, &data) {
                Some(stamp) => {
                    info.intact = true;
                    info.intact_msg = "The document has not been changed since this timestamp.".into();
                    info.timestamp = Some(show_time(stamp.time));
                    if let Some(cert) = &stamp.cert {
                        fill_cert(&mut info, cert);
                        judge_trust(&mut info, cert, &stamp.certs, Some(stamp.time), true);
                    }
                }
                None => info.intact_msg = "The timestamp does not match the document.".into(),
            }
        }
        "adbe.pkcs7.detached" | "ETSI.CAdES.detached" | "" => {
            let Some(sd) = parsed else {
                info.intact_msg = "The signature is damaged and cannot be read.".into();
                return info;
            };
            let checked = CmsContentInfo::from_der(pkcs7)
                .and_then(|mut cms| cms.verify(None, None, Some(&data), None, CMSOptions::NO_SIGNER_CERT_VERIFY | CMSOptions::BINARY));
            match checked {
                Ok(()) => {
                    info.intact = true;
                    info.intact_msg = if info.whole {
                        "The document has not been changed since it was signed.".into()
                    } else {
                        "The signed part has not been changed. More was added to the file after this signature (for example a later signature).".into()
                    };
                }
                Err(e) => info.intact_msg = describe_failure(&e),
            }
            let stamped = embedded_timestamp(&sd);
            info.timestamp = stamped.map(show_time);
            match signer_of(&sd) {
                Some(cert) => {
                    fill_cert(&mut info, &cert);
                    judge_trust(&mut info, &cert, &sd.certs, stamped.or(claimed), stamped.is_some());
                }
                None if info.intact => info.trust_msg = "The signer's certificate is missing from the signature.".into(),
                None => {}
            }
        }
        other => info.intact_msg = format!("Signature type {other} is not supported."),
    }
    info
}

// ---------------------------------------------------------------------------
// Signing
// ---------------------------------------------------------------------------

/// A certificate with its private key, from a .p12 / .pfx file.
pub struct Cert {
    name: String,
    cert: X509,
    key: PKey<Private>,
    /// Issuing certificates included so that others can check the signature.
    chain: Vec<X509>,
}

impl Cert {
    pub fn name(&self) -> String {
        self.name.clone()
    }
}

/// Open a certificate file (.p12 / .pfx) with its password.
pub fn load_p12(path: &Path, password: &str) -> Result<Cert, String> {
    let shown = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let der = std::fs::read(path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
    let p12 = Pkcs12::from_der(&der).map_err(|_| format!("{shown} is not a certificate file (.p12 or .pfx)."))?;
    let parsed = match p12.parse2(password) {
        Ok(p) => p,
        Err(_) => {
            // Files exported by older Windows versions use RC2 encryption, which
            // OpenSSL 3 only reads with its "legacy" module loaded.
            static LEGACY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let loaded = *LEGACY.get_or_init(|| openssl::provider::Provider::try_load(None, "legacy", true).map(std::mem::forget).is_ok());
            match loaded.then(|| p12.parse2(password)) {
                Some(Ok(p)) => p,
                _ => return Err(format!("Cannot open {shown}: the password is wrong, or the file is damaged.")),
            }
        }
    };
    let (Some(key), Some(cert)) = (parsed.pkey, parsed.cert) else {
        return Err(format!("{shown} does not hold a certificate with its private key."));
    };
    let now = chrono::Utc::now().timestamp();
    if unix_time(cert.not_after()).is_some_and(|t| t < now) {
        return Err(format!("The certificate in {shown} has expired."));
    }
    if unix_time(cert.not_before()).is_some_and(|t| t > now) {
        return Err(format!("The certificate in {shown} is not valid yet."));
    }
    let self_signed = |c: &X509| c.subject_name().to_der().ok() == c.issuer_name().to_der().ok();
    // The root is left out: verifiers must have it themselves.
    let chain = parsed.ca.map(|s| s.into_iter().filter(|c| !self_signed(c)).collect()).unwrap_or_default();
    Ok(Cert { name: display_name(cert.subject_name()), cert, key, chain })
}

/// Create the CMS signature over the two signed parts of the file.
pub(super) fn sign_parts(cert: &Cert, p1: &[u8], p2: &[u8]) -> Result<Vec<u8>, String> {
    let fail = |e: openssl::error::ErrorStack| format!("Could not sign with this certificate: {e}");
    let data = [p1, p2].concat();
    let mut chain = Stack::new().map_err(fail)?;
    for c in &cert.chain {
        chain.push(c.clone()).map_err(fail)?;
    }
    let cms = CmsContentInfo::sign(
        Some(&cert.cert),
        Some(&cert.key),
        Some(&chain),
        Some(&data),
        CMSOptions::DETACHED | CMSOptions::BINARY | CMSOptions::NOSMIMECAP,
    )
    .map_err(fail)?;
    cms.to_der().map_err(fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn der_reading() {
        // SEQUENCE { OID 1.2.840.113549.1.9.16.2.14, SET { INTEGER 5 } }
        let der = [0x30, 0x13, 0x06, 0x0B, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x10, 0x02, 0x0E, 0x31, 0x04, 0x02, 0x02, 0x00, 0x05];
        let (seq, rest) = tlv(&der).unwrap();
        assert!(rest.is_empty());
        let f = children(seq.body);
        assert_eq!(oid_string(f[0].body), OID_TIMESTAMP_TOKEN);
        assert_eq!(children(f[1].body)[0].body, &[0x00, 0x05]);
        // The same with indefinite lengths (BER).
        let ber = [0x30, 0x80, 0x31, 0x80, 0x02, 0x01, 0x07, 0x00, 0x00, 0x00, 0x00, 0xFF];
        let (seq, rest) = tlv(&ber).unwrap();
        assert_eq!(rest, &[0xFF]);
        assert_eq!(children(children(seq.body)[0].body)[0].body, &[0x07]);
    }
}
