//! Things on the pages that are not part of the picture: links, the contents
//! tree, comments and form fields. Read once per document version, off the UI thread.

use crate::edit::{self, user_to_view};
use crate::sign::decode_text;
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::{HashMap, HashSet};

pub enum Target {
    /// Page index and the distance from the top of that page, in points.
    Page(usize, f32),
    Url(String),
}

pub struct Link {
    /// In viewer coordinates: points from the top-left of the page as displayed.
    pub rect: [f32; 4],
    pub target: Target,
}

pub struct Note {
    pub id: ObjectId,
    pub rect: [f32; 4],
    pub highlight: bool,
    pub text: String,
    pub author: String,
}

pub struct OutlineItem {
    pub title: String,
    pub level: usize,
    pub page: Option<usize>,
    pub y: f32,
}

pub enum WidgetKind {
    Text { value: String, multiline: bool, max_len: Option<usize>, size: f32 },
    Check { on: String, checked: bool },
    Radio { on: String, checked: bool },
    /// Options are (stored value, shown text).
    Choice { value: String, options: Vec<(String, String)> },
}

pub struct Widget {
    pub id: ObjectId,
    /// The field this widget belongs to (several widgets can share one field).
    pub field: ObjectId,
    pub name: String,
    pub rect: [f32; 4],
    pub kind: WidgetKind,
}

#[derive(Default)]
pub struct PageInfo {
    pub links: Vec<Link>,
    pub notes: Vec<Note>,
    pub widgets: Vec<Widget>,
}

#[derive(Default)]
pub struct DocInfo {
    pub pages: Vec<PageInfo>,
    pub outline: Vec<OutlineItem>,
}

impl DocInfo {
    pub fn widget(&self, id: ObjectId) -> Option<&Widget> {
        self.pages.iter().flat_map(|p| p.widgets.iter()).find(|w| w.id == id)
    }

    /// Text and choice widgets in reading order, for moving on with Tab.
    pub fn next_text_widget(&self, after: ObjectId) -> Option<ObjectId> {
        let all: Vec<ObjectId> = self
            .pages
            .iter()
            .flat_map(|p| p.widgets.iter())
            .filter(|w| matches!(w.kind, WidgetKind::Text { .. }))
            .map(|w| w.id)
            .collect();
        let at = all.iter().position(|id| *id == after)?;
        all.get(at + 1).copied()
    }
}

struct Ctx<'a> {
    doc: &'a Document,
    index: HashMap<ObjectId, usize>,
    boxes: Vec<([f32; 4], i64)>,
    named: HashMap<Vec<u8>, Object>,
}

fn deref<'a>(doc: &'a Document, o: &'a Object) -> &'a Object {
    doc.dereference(o).map(|(_, o)| o).unwrap_or(o)
}

fn dict_of<'a>(doc: &'a Document, o: &'a Object) -> Option<&'a Dictionary> {
    deref(doc, o).as_dict().ok()
}

fn text_of(doc: &Document, o: &Object) -> String {
    match deref(doc, o) {
        Object::String(bytes, _) => decode_text(bytes),
        Object::Name(n) => String::from_utf8_lossy(n).to_string(),
        _ => String::new(),
    }
}

fn nums(doc: &Document, o: &Object) -> Vec<f32> {
    match deref(doc, o) {
        Object::Array(a) => a.iter().filter_map(|v| edit::num(deref(doc, v))).collect(),
        _ => Vec::new(),
    }
}

/// A value from this dictionary or, failing that, from its /Parent chain.
fn inherit<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a Object> {
    let mut cur = dict;
    for _ in 0..32 {
        if let Ok(v) = cur.get(key) {
            return Some(v);
        }
        cur = dict_of(doc, cur.get(b"Parent").ok()?)?;
    }
    None
}

pub fn extract(bytes: &[u8], password: &str) -> Option<DocInfo> {
    let doc = edit::load(bytes, password).ok()?;
    let page_ids: Vec<ObjectId> = doc.page_iter().collect();
    let cx = Ctx {
        index: page_ids.iter().enumerate().map(|(i, id)| (*id, i)).collect(),
        boxes: page_ids.iter().map(|id| edit::page_box(&doc, *id).unwrap_or(([0.0, 0.0, 595.0, 842.0], 0))).collect(),
        named: named_dests(&doc),
        doc: &doc,
    };
    let form_defaults = doc.catalog().ok().and_then(|c| c.get(b"AcroForm").ok()).and_then(|o| dict_of(&doc, o));

    let mut info = DocInfo::default();
    for (i, pid) in page_ids.iter().enumerate() {
        let mut page = PageInfo::default();
        let annots = doc.get_dictionary(*pid).ok().and_then(|d| d.get(b"Annots").ok()).map(|o| deref(&doc, o));
        if let Some(Object::Array(list)) = annots {
            for entry in list {
                let id = entry.as_reference().ok();
                let Some(a) = dict_of(&doc, entry) else { continue };
                // Hidden annotations are not shown and not acted on.
                if a.get(b"F").ok().and_then(|f| f.as_i64().ok()).is_some_and(|f| f & 2 != 0) {
                    continue;
                }
                let r = a.get(b"Rect").map(|o| nums(&doc, o)).unwrap_or_default();
                if r.len() != 4 {
                    continue;
                }
                let (bbox, rot) = cx.boxes[i];
                let (ax, ay) = user_to_view(bbox, rot, r[0], r[1]);
                let (bx, by) = user_to_view(bbox, rot, r[2], r[3]);
                let rect = [ax.min(bx), ay.min(by), ax.max(bx), ay.max(by)];
                let subtype = a.get(b"Subtype").and_then(Object::as_name).unwrap_or(b"");
                match subtype {
                    b"Link" => {
                        if let Some(target) = link_target(&cx, a) {
                            page.links.push(Link { rect, target });
                        }
                    }
                    b"Widget" => {
                        if let Some(id) = id {
                            if let Some(w) = widget(&doc, id, a, rect, form_defaults) {
                                page.widgets.push(w);
                            }
                        }
                    }
                    b"Popup" => {}
                    _ => {
                        let text = a.get(b"Contents").map(|o| text_of(&doc, o)).unwrap_or_default();
                        let highlight = subtype == b"Highlight";
                        if let Some(id) = id {
                            if highlight || subtype == b"Text" || !text.trim().is_empty() {
                                let author = a.get(b"T").map(|o| text_of(&doc, o)).unwrap_or_default();
                                page.notes.push(Note { id, rect, highlight, text, author });
                            }
                        }
                    }
                }
            }
        }
        info.pages.push(page);
    }

    if let Some(first) = doc.catalog().ok().and_then(|c| c.get(b"Outlines").ok()).and_then(|o| dict_of(&doc, o)).and_then(|o| o.get(b"First").ok()) {
        let mut seen = HashSet::new();
        outline(&cx, first, 0, &mut seen, &mut info.outline);
    }
    Some(info)
}

fn outline(cx: &Ctx, first: &Object, level: usize, seen: &mut HashSet<ObjectId>, out: &mut Vec<OutlineItem>) {
    let mut next = Some(first);
    while let Some(o) = next {
        if level > 24 || out.len() > 20_000 {
            return;
        }
        if let Ok(id) = o.as_reference() {
            if !seen.insert(id) {
                return;
            }
        }
        let Some(item) = dict_of(cx.doc, o) else { return };
        let title = item.get(b"Title").map(|t| text_of(cx.doc, t)).unwrap_or_default();
        let dest = item
            .get(b"Dest")
            .ok()
            .and_then(|d| resolve_dest(cx, d, 0))
            .or_else(|| item.get(b"A").ok().and_then(|a| dict_of(cx.doc, a)).and_then(|a| a.get(b"D").ok()).and_then(|d| resolve_dest(cx, d, 0)));
        out.push(OutlineItem {
            title: title.replace(['\r', '\n'], " ").trim().to_string(),
            level,
            page: dest.map(|d| d.0),
            y: dest.map_or(0.0, |d| d.1),
        });
        if let Ok(child) = item.get(b"First") {
            outline(cx, child, level + 1, seen, out);
        }
        next = item.get(b"Next").ok();
    }
}

fn link_target(cx: &Ctx, a: &Dictionary) -> Option<Target> {
    if let Ok(dest) = a.get(b"Dest") {
        return resolve_dest(cx, dest, 0).map(|(p, y)| Target::Page(p, y));
    }
    let action = dict_of(cx.doc, a.get(b"A").ok()?)?;
    match action.get(b"S").and_then(Object::as_name).ok()? {
        b"URI" => {
            let url = text_of(cx.doc, action.get(b"URI").ok()?);
            (!url.trim().is_empty()).then(|| Target::Url(url.trim().to_string()))
        }
        b"GoTo" => resolve_dest(cx, action.get(b"D").ok()?, 0).map(|(p, y)| Target::Page(p, y)),
        _ => None,
    }
}

/// Turn a destination (an array, or the name of one) into a page and a height on it.
fn resolve_dest(cx: &Ctx, dest: &Object, depth: usize) -> Option<(usize, f32)> {
    if depth > 4 {
        return None;
    }
    match deref(cx.doc, dest) {
        Object::String(name, _) | Object::Name(name) => resolve_dest(cx, cx.named.get(name)?, depth + 1),
        Object::Dictionary(d) => resolve_dest(cx, d.get(b"D").ok()?, depth + 1),
        Object::Array(a) => {
            let page = match a.first()? {
                Object::Reference(id) => *cx.index.get(id)?,
                Object::Integer(n) => (*n).max(0) as usize,
                _ => return None,
            };
            let (bbox, rot) = *cx.boxes.get(page)?;
            let kind = a.get(1).and_then(|k| k.as_name().ok()).unwrap_or(b"Fit");
            let value = |i: usize| a.get(i).and_then(|v| edit::num(deref(cx.doc, v)));
            let (left, top) = match kind {
                b"XYZ" => (value(2), value(3)),
                b"FitH" | b"FitBH" => (None, value(2)),
                b"FitR" => (value(2), value(5)),
                _ => (None, None),
            };
            let y = match top {
                Some(top) => user_to_view(bbox, rot, left.unwrap_or(bbox[0]), top).1.max(0.0),
                None => 0.0,
            };
            Some((page, y))
        }
        _ => None,
    }
}

fn named_dests(doc: &Document) -> HashMap<Vec<u8>, Object> {
    let mut out = HashMap::new();
    let Ok(catalog) = doc.catalog() else { return out };
    if let Some(d) = catalog.get(b"Dests").ok().and_then(|o| dict_of(doc, o)) {
        for (k, v) in d.iter() {
            out.insert(k.clone(), v.clone());
        }
    }
    if let Some(tree) = catalog.get(b"Names").ok().and_then(|o| dict_of(doc, o)).and_then(|n| n.get(b"Dests").ok()) {
        let mut seen = HashSet::new();
        name_tree(doc, tree, 0, &mut seen, &mut out);
    }
    out
}

fn name_tree(doc: &Document, node: &Object, depth: usize, seen: &mut HashSet<ObjectId>, out: &mut HashMap<Vec<u8>, Object>) {
    if depth > 32 {
        return;
    }
    if let Ok(id) = node.as_reference() {
        if !seen.insert(id) {
            return;
        }
    }
    let Some(d) = dict_of(doc, node) else { return };
    if let Some(Object::Array(names)) = d.get(b"Names").ok().map(|o| deref(doc, o)) {
        for pair in names.chunks(2) {
            if let [key, value] = pair {
                if let Object::String(k, _) = deref(doc, key) {
                    out.insert(k.clone(), value.clone());
                }
            }
        }
    }
    if let Some(Object::Array(kids)) = d.get(b"Kids").ok().map(|o| deref(doc, o)) {
        for kid in kids {
            name_tree(doc, kid, depth + 1, seen, out);
        }
    }
}

/// The name of a checkbox or radio button's "on" appearance.
pub(crate) fn on_state(doc: &Document, widget: &Dictionary) -> Option<String> {
    let ap = dict_of(doc, widget.get(b"AP").ok()?)?;
    let normal = dict_of(doc, ap.get(b"N").ok()?)?;
    normal.iter().map(|(k, _)| k).find(|k| k.as_slice() != b"Off").map(|k| String::from_utf8_lossy(k).to_string())
}

fn widget(doc: &Document, id: ObjectId, w: &Dictionary, rect: [f32; 4], defaults: Option<&Dictionary>) -> Option<Widget> {
    let kind = inherit(doc, w, b"FT")?.as_name().ok()?;
    let flags = inherit(doc, w, b"Ff").and_then(|f| deref(doc, f).as_i64().ok()).unwrap_or(0);
    if flags & 1 != 0 {
        return None; // read-only
    }
    // The field is the nearest dictionary with a name; a widget without one belongs to its parent.
    let mut field = id;
    let mut cur = w;
    let mut names = Vec::new();
    for _ in 0..32 {
        if let Ok(t) = cur.get(b"T") {
            names.push(text_of(doc, t));
        }
        let Ok(parent) = cur.get(b"Parent") else { break };
        if !cur.has(b"T") {
            if let Ok(pid) = parent.as_reference() {
                field = pid;
            }
        }
        let Some(p) = dict_of(doc, parent) else { break };
        cur = p;
    }
    names.reverse();
    let value = inherit(doc, w, b"V");
    let value_text = |v: Option<&Object>| match v.map(|v| deref(doc, v)) {
        Some(Object::Array(a)) => a.first().map(|f| text_of(doc, f)).unwrap_or_default(),
        Some(o) => text_of(doc, o),
        None => String::new(),
    };

    let kind = match kind {
        b"Tx" => {
            if flags & (1 << 13) != 0 {
                return None; // password field
            }
            let da = inherit(doc, w, b"DA").or_else(|| defaults.and_then(|d| d.get(b"DA").ok())).map(|o| text_of(doc, o)).unwrap_or_default();
            WidgetKind::Text {
                value: value_text(value),
                multiline: flags & (1 << 12) != 0,
                max_len: inherit(doc, w, b"MaxLen").and_then(|m| deref(doc, m).as_i64().ok()).filter(|m| *m > 0).map(|m| m as usize),
                size: da_size(&da),
            }
        }
        b"Btn" => {
            if flags & (1 << 16) != 0 {
                return None; // push button
            }
            let on = on_state(doc, w)?;
            let state = w.get(b"AS").ok().map(|s| text_of(doc, s)).unwrap_or_else(|| value_text(value));
            let checked = state == on;
            if flags & (1 << 15) != 0 { WidgetKind::Radio { on, checked } } else { WidgetKind::Check { on, checked } }
        }
        b"Ch" => {
            let mut options = Vec::new();
            if let Some(Object::Array(list)) = inherit(doc, w, b"Opt").map(|o| deref(doc, o)) {
                for o in list {
                    match deref(doc, o) {
                        Object::Array(pair) if pair.len() >= 2 => options.push((text_of(doc, &pair[0]), text_of(doc, &pair[1]))),
                        other => {
                            let t = text_of(doc, other);
                            options.push((t.clone(), t));
                        }
                    }
                }
            }
            WidgetKind::Choice { value: value_text(value), options }
        }
        _ => return None,
    };
    Some(Widget { id, field, name: names.join("."), rect, kind })
}

/// The font size in a default appearance string such as `/Helv 10 Tf 0 g` (0 = automatic).
pub(crate) fn da_size(da: &str) -> f32 {
    let parts: Vec<&str> = da.split_whitespace().collect();
    parts.iter().position(|p| *p == "Tf").and_then(|i| i.checked_sub(1)).and_then(|i| parts[i].parse::<f32>().ok()).unwrap_or(0.0)
}
