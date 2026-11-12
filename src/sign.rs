//! Digital signatures: checking the ones in a PDF and adding a new one.
//!
//! Finding the signatures in the file and building the signed file is the same
//! everywhere. The cryptography is done by the system:
//! - Windows: CryptoAPI. That is what makes signature cards and USB tokens
//!   work: their certificates appear in the Windows certificate store and
//!   Windows asks for the PIN. Trust is decided by the Windows list of trusted
//!   root certificates. See `sign_windows.rs`.
//! - Linux: OpenSSL. Trust is decided by the system's list of certificate
//!   authorities plus any the user adds, and documents are signed with a
//!   certificate file (.p12 / .pfx). See `sign_openssl.rs`.

use crate::edit;
use lopdf::{Dictionary, Document, IncrementalDocument, Object, ObjectId, Stream, StringFormat, dictionary};

#[cfg_attr(windows, path = "sign_windows.rs")]
#[cfg_attr(not(windows), path = "sign_openssl.rs")]
mod backend;

#[cfg(windows)]
pub use backend::{find_certificate, pick_certificate};
#[cfg(not(windows))]
pub use backend::load_p12;
pub use backend::Cert;

/// Bytes kept free in the file for the signature itself.
const RESERVE: usize = 16384;

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

pub(crate) fn decode_text(raw: &[u8]) -> String {
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
    scan(bytes).iter().map(|raw| backend::verify(bytes, raw)).collect()
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

            let label = if backend::ui_is_bulgarian() { "Електронно подписано от" } else { "Digitally signed by" };
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

    let signature = backend::sign_parts(cert, &out[..lt], &out[gt + 1..])?;
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
