//! Document edits. Every edit takes the current file bytes and returns new
//! bytes, which keeps undo trivial (the previous bytes are simply kept).

use lopdf::{Dictionary, Document, LoadOptions, Object, ObjectId, Stream, dictionary};
use std::collections::BTreeMap;

pub type Res<T> = Result<T, String>;

pub enum Op {
    /// Rotate the given pages (0-based) by a multiple of 90 degrees clockwise.
    Rotate { pages: Vec<usize>, degrees: i32 },
    Delete { pages: Vec<usize> },
    Move { from: usize, to: usize },
    /// Insert every page of another PDF before page index `at`.
    Insert { at: usize, other: Vec<u8> },
    /// Keep only the given pages (used for "save pages as").
    Keep { pages: Vec<usize> },
    /// Write text on a page. `x`/`y` are the top-left corner of the first line
    /// in viewer coordinates (points from the top-left of the page as displayed).
    AddText { page: usize, x: f32, y: f32, text: String, size: f32, color: [f32; 3] },
}

fn err<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> String {
    move |e| format!("{what}: {e}")
}

fn load(bytes: &[u8], password: &str) -> Res<Document> {
    let mut doc = if password.is_empty() {
        Document::load_mem(bytes)
    } else {
        Document::load_mem_with_options(bytes, LoadOptions::with_password(password))
    }
    .map_err(err("Cannot read the PDF for editing"))?;
    // The edited copy is written without encryption.
    if doc.encryption_state.is_some() || doc.trailer.has(b"Encrypt") {
        if doc.encryption_state.is_none() {
            return Err("This PDF is password protected and cannot be edited.".into());
        }
        doc.encryption_state = None;
        doc.trailer.remove(b"Encrypt");
    }
    Ok(doc)
}

fn save(doc: &mut Document) -> Res<Vec<u8>> {
    let mut out = Vec::new();
    doc.save_to(&mut out).map_err(err("Cannot write the PDF"))?;
    Ok(out)
}

pub fn apply(bytes: &[u8], password: &str, op: Op) -> Res<Vec<u8>> {
    let mut doc = load(bytes, password)?;
    let (root, mut pages) = flatten(&mut doc)?;
    let n = pages.len();
    let check = |i: usize| if i < n { Ok(i) } else { Err("Page out of range".to_string()) };

    match op {
        Op::Rotate { pages: which, degrees } => {
            for i in which {
                let id = pages[check(i)?];
                let d = doc.get_dictionary_mut(id).map_err(err("Page"))?;
                let cur = d.get(b"Rotate").ok().and_then(|o| o.as_i64().ok()).unwrap_or(0);
                d.set("Rotate", (cur + degrees as i64).rem_euclid(360));
            }
        }
        Op::Delete { pages: mut which } => {
            which.sort_unstable();
            which.dedup();
            if which.len() >= n {
                return Err("A PDF needs at least one page.".into());
            }
            for i in which.into_iter().rev() {
                pages.remove(check(i)?);
            }
            set_kids(&mut doc, root, &pages)?;
            doc.prune_objects();
        }
        Op::Move { from, to } => {
            let id = pages.remove(check(from)?);
            pages.insert(to.min(pages.len()), id);
            set_kids(&mut doc, root, &pages)?;
        }
        Op::Keep { pages: which } => {
            let mut kept = Vec::new();
            for i in which {
                kept.push(pages[check(i)?]);
            }
            if kept.is_empty() {
                return Err("No pages selected.".into());
            }
            set_kids(&mut doc, root, &kept)?;
            // Navigation structures would keep every other page alive.
            if let Ok(cat) = doc.catalog_mut() {
                for key in ["Outlines", "Names", "Dests", "StructTreeRoot", "OpenAction", "PageLabels", "Threads"] {
                    cat.remove(key.as_bytes());
                }
            }
            doc.prune_objects();
        }
        Op::Insert { at, other } => {
            let mut other = load(&other, "")?;
            let (_, _) = flatten(&mut other)?;
            other.renumber_objects_with(doc.max_id + 1);
            let other_pages: Vec<ObjectId> = other.page_iter().collect();
            if other_pages.is_empty() {
                return Err("The other PDF has no pages.".into());
            }
            if other.version > doc.version {
                doc.version = other.version.clone();
            }
            doc.max_id = doc.max_id.max(other.max_id);
            for (id, obj) in std::mem::take(&mut other.objects) {
                let is_tree = match &obj {
                    Object::Dictionary(d) => {
                        matches!(d.get(b"Type").and_then(Object::as_name), Ok(b"Catalog") | Ok(b"Pages"))
                    }
                    _ => false,
                };
                if !is_tree {
                    doc.max_id = doc.max_id.max(id.0);
                    doc.objects.insert(id, obj);
                }
            }
            let at = at.min(pages.len());
            for (k, id) in other_pages.iter().enumerate() {
                doc.get_dictionary_mut(*id).map_err(err("Inserted page"))?.set("Parent", root);
                pages.insert(at + k, *id);
            }
            set_kids(&mut doc, root, &pages)?;
        }
        Op::AddText { page, x, y, text, size, color } => {
            add_text(&mut doc, pages[check(page)?], x, y, &text, size, color)?;
        }
    }
    save(&mut doc)
}

/// Turn the page tree into one flat list under the root node, copying inherited
/// attributes onto each page, so that pages can be reordered freely.
fn flatten(doc: &mut Document) -> Res<(ObjectId, Vec<ObjectId>)> {
    let pages: Vec<ObjectId> = doc.page_iter().collect();
    if pages.is_empty() {
        return Err("The PDF has no pages.".into());
    }
    let root = doc
        .catalog()
        .and_then(|c| c.get(b"Pages"))
        .and_then(Object::as_reference)
        .map_err(err("Page tree"))?;

    for &p in &pages {
        for key in [&b"Resources"[..], b"MediaBox", b"CropBox", b"Rotate"] {
            let has = doc.get_dictionary(p).map(|d| d.has(key)).unwrap_or(true);
            if !has {
                if let Some(v) = inherited(doc, p, key) {
                    if let Ok(d) = doc.get_dictionary_mut(p) {
                        d.set(key, v);
                    }
                }
            }
        }
        doc.get_dictionary_mut(p).map_err(err("Page"))?.set("Parent", root);
    }
    set_kids(doc, root, &pages)?;
    Ok((root, pages))
}

fn inherited(doc: &Document, page: ObjectId, key: &[u8]) -> Option<Object> {
    let mut id = page;
    for _ in 0..64 {
        let d = doc.get_dictionary(id).ok()?;
        if let Ok(v) = d.get(key) {
            return Some(v.clone());
        }
        id = d.get(b"Parent").ok()?.as_reference().ok()?;
    }
    None
}

fn set_kids(doc: &mut Document, root: ObjectId, pages: &[ObjectId]) -> Res<()> {
    let d = doc.get_dictionary_mut(root).map_err(err("Page tree"))?;
    d.set("Kids", pages.iter().map(|id| Object::Reference(*id)).collect::<Vec<_>>());
    d.set("Count", pages.len() as i64);
    Ok(())
}

fn num(o: &Object) -> Option<f32> {
    match o {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r),
        _ => None,
    }
}

fn rect_of(doc: &Document, page: &Dictionary, key: &[u8]) -> Option<[f32; 4]> {
    let (_, o) = doc.dereference(page.get(key).ok()?).ok()?;
    let a = o.as_array().ok()?;
    if a.len() != 4 {
        return None;
    }
    let mut v = [0.0; 4];
    for (i, o) in a.iter().enumerate() {
        v[i] = num(doc.dereference(o).ok()?.1)?;
    }
    Some([v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])])
}

/// The visible box of a page and its rotation.
fn page_box(doc: &Document, id: ObjectId) -> Res<([f32; 4], i64)> {
    let page = doc.get_dictionary(id).map_err(err("Page"))?;
    let media = rect_of(doc, page, b"MediaBox").unwrap_or([0.0, 0.0, 595.0, 842.0]);
    let crop = rect_of(doc, page, b"CropBox").unwrap_or(media);
    let mut b = [crop[0].max(media[0]), crop[1].max(media[1]), crop[2].min(media[2]), crop[3].min(media[3])];
    if b[2] - b[0] < 1.0 || b[3] - b[1] < 1.0 {
        b = media;
    }
    let rot = page
        .get(b"Rotate")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_i64().ok())
        .unwrap_or(0)
        .rem_euclid(360);
    Ok((b, rot))
}

/// Map a point in viewer coordinates to PDF user space, and give the text
/// matrix direction that reads left to right on screen.
fn view_to_user(b: [f32; 4], rot: i64, vx: f32, vy: f32) -> ((f32, f32), [f32; 4]) {
    let (x0, y0) = (b[0], b[1]);
    let (w, h) = (b[2] - b[0], b[3] - b[1]);
    match rot {
        90 => ((x0 + vy, y0 + vx), [0.0, 1.0, -1.0, 0.0]),
        180 => ((x0 + w - vx, y0 + vy), [-1.0, 0.0, 0.0, -1.0]),
        270 => ((x0 + w - vy, y0 + h - vx), [0.0, -1.0, 1.0, 0.0]),
        _ => ((x0 + vx, y0 + h - vy), [1.0, 0.0, 0.0, 1.0]),
    }
}

struct EmbeddedFont {
    id: ObjectId,
    /// New glyph id for each character used.
    gids: BTreeMap<char, u16>,
}

fn add_text(doc: &mut Document, page: ObjectId, x: f32, y: f32, text: &str, size: f32, color: [f32; 3]) -> Res<()> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ");
    if text.trim().is_empty() {
        return Err("No text to add.".into());
    }
    let (bbox, rot) = page_box(doc, page)?;
    let font = embed_font(doc, &text)?;
    let font_name = format!("PPdfF{}", font.id.0);

    let mut ops = String::new();
    ops.push_str("Q\nq\nBT\n");
    ops.push_str(&format!("/{font_name} {size:.2} Tf\n{:.3} {:.3} {:.3} rg\n", color[0], color[1], color[2]));
    for (i, line) in text.split('\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        // First baseline sits about one ascent below the clicked point.
        let vy = y + size * 0.92 + i as f32 * size * 1.2;
        let ((ux, uy), m) = view_to_user(bbox, rot, x, vy);
        ops.push_str(&format!("{} {} {} {} {ux:.2} {uy:.2} Tm\n<", m[0], m[1], m[2], m[3]));
        for c in line.chars() {
            let gid = font.gids.get(&c).copied().unwrap_or(0);
            ops.push_str(&format!("{gid:04X}"));
        }
        ops.push_str("> Tj\n");
    }
    ops.push_str("ET\nQ\n");

    // The existing content is wrapped in q ... Q so that whatever graphics
    // state it leaves behind cannot move or recolor the added text.
    let open = doc.add_object(Stream::new(dictionary! {}, b"q\n".to_vec()));
    let close = doc.add_object(Stream::new(dictionary! {}, ops.into_bytes()));
    let mut contents = vec![Object::Reference(open)];
    let existing = doc.get_dictionary(page).map_err(err("Page"))?.get(b"Contents").ok().cloned();
    match existing {
        Some(Object::Reference(id)) => match doc.get_object(id) {
            Ok(Object::Array(a)) => contents.extend(a.iter().cloned()),
            Ok(_) => contents.push(Object::Reference(id)),
            Err(_) => {}
        },
        Some(Object::Array(a)) => contents.extend(a),
        _ => {}
    }
    contents.push(Object::Reference(close));

    // Resources are copied rather than edited in place: they are often shared between pages.
    let mut res = match doc.get_dictionary(page).map_err(err("Page"))?.get(b"Resources") {
        Ok(o) => doc.dereference(o).ok().and_then(|(_, o)| o.as_dict().ok().cloned()).unwrap_or_default(),
        Err(_) => Dictionary::new(),
    };
    let mut fonts = match res.get(b"Font") {
        Ok(o) => doc.dereference(o).ok().and_then(|(_, o)| o.as_dict().ok().cloned()).unwrap_or_default(),
        Err(_) => Dictionary::new(),
    };
    fonts.set(font_name, font.id);
    res.set("Font", fonts);

    let d = doc.get_dictionary_mut(page).map_err(err("Page"))?;
    d.set("Contents", contents);
    d.set("Resources", res);
    Ok(())
}

/// Embed a subset of a Windows font that covers `text` as an Identity-H
/// composite font, so that any script (Cyrillic included) can be written.
fn embed_font(doc: &mut Document, text: &str) -> Res<EmbeddedFont> {
    let data = ["arial.ttf", "segoeui.ttf", "tahoma.ttf", "verdana.ttf"]
        .iter()
        .find_map(|f| std::fs::read(crate::fonts_dir().join(f)).ok())
        .ok_or("No usable font found in the Windows Fonts folder.")?;
    let face = ttf_parser::Face::parse(&data, 0).map_err(err("Font"))?;
    let upem = face.units_per_em().max(1) as f32;
    let k = 1000.0 / upem;

    let mut remap = subsetter::GlyphRemapper::new();
    remap.remap(0);
    let mut gids = BTreeMap::new();
    let mut widths: BTreeMap<u16, f32> = BTreeMap::new();
    widths.insert(0, face.glyph_hor_advance(ttf_parser::GlyphId(0)).unwrap_or(0) as f32 * k);
    for c in text.chars().filter(|c| *c != '\n') {
        if gids.contains_key(&c) {
            continue;
        }
        let old = face.glyph_index(c).unwrap_or(ttf_parser::GlyphId(0));
        let new = remap.remap(old.0);
        gids.insert(c, new);
        widths.insert(new, face.glyph_hor_advance(old).unwrap_or(0) as f32 * k);
    }
    let subset = subsetter::subset(&data, 0, &remap).map_err(|e| format!("Font subset: {e:?}"))?;

    // Six letters derived from the text keep subset names distinct.
    let mut h: u32 = 2166136261;
    for b in text.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    let tag: String = (0..6).map(|i| (b'A' + ((h >> (i * 5)) % 26) as u8) as char).collect();
    let ps = face
        .names()
        .into_iter()
        .find(|n| n.name_id == ttf_parser::name_id::POST_SCRIPT_NAME)
        .and_then(|n| n.to_string())
        .map(|s| s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ArialMT".to_string());
    let base = format!("{tag}+{ps}");

    let mut file = Stream::new(dictionary! { "Length1" => subset.len() as i64 }, subset);
    let _ = file.compress();
    let file_id = doc.add_object(file);

    let bb = face.global_bounding_box();
    let descriptor = doc.add_object(dictionary! {
        "Type" => "FontDescriptor",
        "FontName" => base.as_str(),
        "Flags" => 4,
        "FontBBox" => vec![
            Object::Real(bb.x_min as f32 * k), Object::Real(bb.y_min as f32 * k),
            Object::Real(bb.x_max as f32 * k), Object::Real(bb.y_max as f32 * k),
        ],
        "ItalicAngle" => 0,
        "Ascent" => Object::Real(face.ascender() as f32 * k),
        "Descent" => Object::Real(face.descender() as f32 * k),
        "CapHeight" => Object::Real(face.capital_height().unwrap_or(face.ascender()) as f32 * k),
        "StemV" => 80,
        "FontFile2" => file_id,
    });

    let n = remap.num_gids();
    let w: Vec<Object> = (0..n).map(|g| Object::Real(widths.get(&g).copied().unwrap_or(0.0).round())).collect();
    let cid = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "CIDFontType2",
        "BaseFont" => base.as_str(),
        "CIDSystemInfo" => dictionary! {
            "Registry" => Object::string_literal("Adobe"),
            "Ordering" => Object::string_literal("Identity"),
            "Supplement" => 0,
        },
        "FontDescriptor" => descriptor,
        "DW" => 1000,
        "W" => vec![Object::Integer(0), Object::Array(w)],
        "CIDToGIDMap" => "Identity",
    });

    let mut cmap = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
         1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    let pairs: Vec<(u16, char)> = gids.iter().filter(|(_, g)| **g != 0).map(|(c, g)| (*g, *c)).collect();
    for chunk in pairs.chunks(100) {
        cmap.push_str(&format!("{} beginbfchar\n", chunk.len()));
        for (g, c) in chunk {
            let mut u = [0u16; 2];
            let hex: String = c.encode_utf16(&mut u).iter().map(|x| format!("{x:04X}")).collect();
            cmap.push_str(&format!("<{g:04X}> <{hex}>\n"));
        }
        cmap.push_str("endbfchar\n");
    }
    cmap.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    let mut to_unicode = Stream::new(dictionary! {}, cmap.into_bytes());
    let _ = to_unicode.compress();
    let to_unicode = doc.add_object(to_unicode);

    let id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type0",
        "BaseFont" => base.as_str(),
        "Encoding" => "Identity-H",
        "DescendantFonts" => vec![Object::Reference(cid)],
        "ToUnicode" => to_unicode,
    });
    Ok(EmbeddedFont { id, gids })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{interpreter_settings, open_pdf, page_text};
    use std::sync::Arc;

    fn sample() -> Vec<u8> {
        let path = std::env::var("PROTOPDF_SAMPLE").expect("set PROTOPDF_SAMPLE to a multi-page PDF");
        std::fs::read(path).unwrap()
    }

    /// Page sizes and page text, read back with the viewer's own parser.
    fn read(bytes: &[u8]) -> Vec<((f32, f32), String)> {
        let pdf = open_pdf(Arc::new(bytes.to_vec()), "").ok().expect("readable PDF");
        let settings = interpreter_settings();
        pdf.pages()
            .iter()
            .map(|p| {
                let t = page_text(p, &settings);
                (p.render_dimensions(), t.text_range(0, t.len()))
            })
            .collect()
    }

    fn out(name: &str, bytes: &[u8]) {
        if let Ok(dir) = std::env::var("PROTOPDF_OUT") {
            std::fs::write(std::path::Path::new(&dir).join(name), bytes).unwrap();
        }
    }

    #[test]
    fn page_operations() {
        let src = sample();
        let before = read(&src);
        let n = before.len();
        assert!(n >= 3, "sample needs 3+ pages");

        let rotated = apply(&src, "", Op::Rotate { pages: vec![0], degrees: 90 }).unwrap();
        let r = read(&rotated);
        assert_eq!(r.len(), n);
        assert!((r[0].0.0 - before[0].0.1).abs() < 0.5 && (r[0].0.1 - before[0].0.0).abs() < 0.5);
        assert_eq!(r[1].0, before[1].0);
        out("rotated.pdf", &rotated);

        let deleted = apply(&src, "", Op::Delete { pages: vec![0] }).unwrap();
        let d = read(&deleted);
        assert_eq!(d.len(), n - 1);
        assert_eq!(d[0].1, before[1].1);

        let moved = apply(&src, "", Op::Move { from: 0, to: 2 }).unwrap();
        let m = read(&moved);
        assert_eq!(m.len(), n);
        assert_eq!(m[2].1, before[0].1);
        assert_eq!(m[0].1, before[1].1);

        let merged = apply(&src, "", Op::Insert { at: 1, other: src.clone() }).unwrap();
        let g = read(&merged);
        assert_eq!(g.len(), 2 * n);
        assert_eq!(g[1].1, before[0].1);
        assert_eq!(g[n].1, before[n - 1].1);
        assert_eq!(g[n + 1].1, before[1].1);
        out("merged.pdf", &merged);

        let kept = apply(&src, "", Op::Keep { pages: vec![2, 0] }).unwrap();
        let k = read(&kept);
        assert_eq!(k.len(), 2);
        assert_eq!(k[0].1, before[2].1);
        assert_eq!(k[1].1, before[0].1);
        out("kept.pdf", &kept);

        assert!(apply(&src, "", Op::Delete { pages: (0..n).collect() }).is_err());
    }

    #[test]
    fn add_text_in_cyrillic() {
        let src = sample();
        let text = "Здравей, ProtoPDF! 123";
        let op = |page| Op::AddText { page, x: 40.0, y: 60.0, text: format!("{text}\nвтори ред"), size: 18.0, color: [0.8, 0.05, 0.05] };
        let edited = apply(&src, "", op(0)).unwrap();
        let pages = read(&edited);
        assert!(pages[0].1.contains(text), "added text is searchable: {:?}", &pages[0].1.chars().rev().take(80).collect::<String>());
        assert!(pages[0].1.contains("втори ред"));
        out("text.pdf", &edited);

        // The same on a page that has been turned: the text must still land upright.
        for deg in [90, 180, 270] {
            let turned = apply(&src, "", Op::Rotate { pages: vec![0], degrees: deg }).unwrap();
            let edited = apply(&turned, "", op(0)).unwrap();
            assert!(read(&edited)[0].1.contains(text));
            out(&format!("text{deg}.pdf"), &edited);
        }
        // Adding a second time keeps the first.
        let twice = apply(&edited, "", Op::AddText { page: 0, x: 40.0, y: 140.0, text: "Още текст".into(), size: 14.0, color: [0.0; 3] }).unwrap();
        let p = read(&twice);
        assert!(p[0].1.contains(text) && p[0].1.contains("Още текст"));
    }
}
