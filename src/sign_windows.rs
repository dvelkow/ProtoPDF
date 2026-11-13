//! Signature cryptography on Windows, through CryptoAPI.

use super::*;
use std::ffi::{CStr, c_void};
use std::ptr::{null, null_mut};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Security::Cryptography::UI::CryptUIDlgSelectCertificateFromStore;
use windows::Win32::Security::Cryptography::*;
use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows::core::{PSTR, w};

const ENC: u32 = X509_ASN_ENCODING.0 | PKCS_7_ASN_ENCODING.0;
const OID_TIMESTAMP_TOKEN: &[u8] = b"1.2.840.113549.1.9.16.2.14";

pub(super) fn verify(bytes: &[u8], raw: &RawSig) -> SigInfo {
    unsafe { verify_one(bytes, raw) }
}

pub(super) fn sign_parts(cert: &Cert, p1: &[u8], p2: &[u8]) -> Result<Vec<u8>, String> {
    unsafe { cms_sign(cert, p1, p2) }
}

pub(super) fn ui_is_bulgarian() -> bool {
    use windows::Win32::Globalization::GetUserDefaultUILanguage;
    unsafe { GetUserDefaultUILanguage() & 0x3ff == 0x02 }
}

fn filetime_to_unix(ft: FILETIME) -> i64 {
    let t = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    (t / 10_000_000) as i64 - 11_644_473_600
}

fn unix_to_filetime(secs: i64) -> FILETIME {
    let t = ((secs + 11_644_473_600) as u64) * 10_000_000;
    FILETIME { dwLowDateTime: t as u32, dwHighDateTime: (t >> 32) as u32 }
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

impl Clone for Cert {
    fn clone(&self) -> Self {
        Cert(unsafe { CertDuplicateCertificateContext(Some(self.0)) })
    }
}

impl Cert {
    pub fn name(&self) -> String {
        unsafe { cert_text(self.0, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, None) }
    }

    /// SHA-1 thumbprint in hex, to find the certificate again with `find_certificate`.
    pub fn thumbprint(&self) -> String {
        let mut hash = [0u8; 20];
        let mut cb = 20u32;
        let ok = unsafe { CertGetCertificateContextProperty(self.0, CERT_SHA1_HASH_PROP_ID, Some(hash.as_mut_ptr().cast()), &mut cb) }.is_ok();
        if ok { hash.iter().map(|b| format!("{b:02X}")).collect() } else { String::new() }
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
