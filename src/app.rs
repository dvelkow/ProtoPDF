//! The window: toolbar, page view, thumbnails, search and dialogs.

use crate::edit::{self, Op};
use crate::print;
use crate::register;
use crate::render::{Job, Kind, OpenError, Out, Pool};
use crate::text::{PageText, lower_chars};
use eframe::egui::{
    self, Align, Align2, Color32, CursorIcon, FontId, Key, Layout as UiLayout, Modifiers, Pos2,
    Rect, ScrollArea, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Vec2,
    ViewportCommand, pos2, vec2,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

/// Screen pixels per PDF point at 100% zoom.
const PT_TO_PX: f32 = 96.0 / 72.0;
const GAP: f32 = 14.0;
const MARGIN: f32 = 18.0;
const THUMB_PX: u32 = 180;
const THUMB_W: f32 = 112.0;
const THUMB_PANEL: f32 = 150.0;
const TEX_BUDGET: usize = 420 << 20;
const ZOOMS: [f32; 16] =
    [0.25, 0.33, 0.5, 0.67, 0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0, 6.0];
const TEXT_COLORS: [(&str, [f32; 3]); 3] =
    [("Black", [0.0, 0.0, 0.0]), ("Blue", [0.05, 0.2, 0.75]), ("Red", [0.8, 0.05, 0.05])];

#[derive(PartialEq, Clone, Copy)]
enum Fit {
    None,
    Width,
    Page,
}

#[derive(PartialEq, Clone, Copy)]
enum Tool {
    Select,
    AddText,
}

struct PageTex {
    tex: TextureHandle,
    width: u32,
    used: u64,
    bytes: usize,
}

type Snap = (Arc<Vec<u8>>, String);

struct Doc {
    path: Option<PathBuf>,
    bytes: Arc<Vec<u8>>,
    saved: Arc<Vec<u8>>,
    password: String,
    pool: Pool,
    /// Page sizes in points. Kept from the previous version while an edited
    /// document is being re-read, so the view does not jump.
    sizes: Vec<(f32, f32)>,
    loaded: bool,
    tex: HashMap<usize, PageTex>,
    thumbs: HashMap<usize, TextureHandle>,
    texts: HashMap<usize, Arc<PageText>>,
    undo: Vec<Snap>,
    redo: Vec<Snap>,
}

impl Doc {
    fn dirty(&self) -> bool {
        !Arc::ptr_eq(&self.bytes, &self.saved)
    }

    fn name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "Untitled.pdf".into())
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Hit {
    page: usize,
    a: usize,
    b: usize,
}

#[derive(Default)]
struct Search {
    open: bool,
    focus: bool,
    query: String,
    active: String,
    hits: Vec<Hit>,
    cur: Option<usize>,
    scanned: HashSet<usize>,
    /// Go to the first hit at or after the current page once one is found.
    seek: bool,
}

struct Sel {
    page: usize,
    anchor: usize,
    head: usize,
}

struct TextDraft {
    page: usize,
    x: f32,
    y: f32,
    text: String,
    size: f32,
    color: usize,
    focus: bool,
    screen: Pos2,
}

enum EditDone {
    Replace(Result<Vec<u8>, String>, Option<usize>, &'static str),
    Export(Result<(), String>, PathBuf),
}

enum Act {
    Open,
    OpenPath(PathBuf),
    Save,
    SaveAs,
    Print,
    Undo,
    Redo,
    Copy,
    SelectAll,
    Find,
    FindNext(bool),
    ZoomStep(bool),
    Zoom(f32),
    Fit(Fit),
    Jump(usize),
    ScrollBy(Vec2),
    ScrollEnd(bool),
    Rotate(i32, bool),
    Delete,
    MovePage(bool),
    InsertPdf(bool),
    ExtractDialog,
    Extract(Vec<usize>),
    ToolAddText,
    AddText,
    ToggleThumbs,
    MakeDefault,
    Password(Option<String>),
    CloseAnswer(u8),
    Quit,
}

struct Layout {
    /// Screen pixels per point.
    ppt: f32,
    tops: Vec<f32>,
    w: f32,
    h: f32,
}

fn layout(sizes: &[(f32, f32)], zoom: f32) -> Layout {
    let ppt = zoom * PT_TO_PX;
    let mut tops = Vec::with_capacity(sizes.len());
    let mut y = GAP;
    let mut max_w = 0.0f32;
    for (w, h) in sizes {
        tops.push(y);
        y += h * ppt + GAP;
        max_w = max_w.max(*w);
    }
    Layout { ppt, tops, w: max_w * ppt + 2.0 * MARGIN, h: y }
}

impl Layout {
    fn page_at(&self, y: f32) -> usize {
        self.tops.partition_point(|&t| t <= y).saturating_sub(1)
    }
}

fn want_width(size: (f32, f32), ppt: f32, ppp: f32, max_side: f32) -> u32 {
    let (w, h) = (size.0 * ppt * ppp, size.1 * ppt * ppp);
    let s = (max_side / w.max(h).max(1.0)).min(1.0);
    let mut v = (w * s).round().max(16.0) as u32;
    if v == THUMB_PX {
        v += 1;
    }
    v
}

pub struct App {
    doc: Option<Doc>,
    zoom: f32,
    fit: Fit,
    init_zoom: bool,
    scroll: Vec2,
    set_scroll: Option<Vec2>,
    jump: Option<usize>,
    view_size: Vec2,
    cur_page: usize,
    page_field: String,
    show_thumbs: bool,
    thumb_follow: usize,
    thumb_view: (f32, f32),
    thumb_scroll: Option<f32>,
    tool: Tool,
    search: Search,
    sel: Option<Sel>,
    draft: Option<TextDraft>,
    password: Option<(String, bool)>,
    extract: Option<String>,
    error: Option<String>,
    status: String,
    edit_rx: Option<Receiver<EditDone>>,
    print_rx: Option<Receiver<print::Progress>>,
    confirm_close: bool,
    force_close: bool,
    jobs_view: Vec<Job>,
    jobs_thumb: Vec<Job>,
    frame: u64,
    title: String,
    shot: Option<PathBuf>,
    shot_wait: u32,
    /// Steps from `--do`, run one at a time once the view has settled (for automated checks).
    script: Vec<String>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, file: Option<PathBuf>, shot: Option<PathBuf>) -> Self {
        // Ctrl +/- zoom the document, not the whole interface.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let mut app = Self {
            doc: None,
            zoom: 1.0,
            fit: Fit::None,
            init_zoom: true,
            scroll: Vec2::ZERO,
            set_scroll: None,
            jump: None,
            view_size: vec2(800.0, 600.0),
            cur_page: 0,
            page_field: String::new(),
            show_thumbs: false,
            thumb_follow: usize::MAX,
            thumb_view: (0.0, 0.0),
            thumb_scroll: None,
            tool: Tool::Select,
            search: Search::default(),
            sel: None,
            draft: None,
            password: None,
            extract: None,
            error: None,
            status: String::new(),
            edit_rx: None,
            print_rx: None,
            confirm_close: false,
            force_close: false,
            jobs_view: Vec::new(),
            jobs_thumb: Vec::new(),
            frame: 0,
            title: String::new(),
            shot,
            shot_wait: 0,
            script: Vec::new(),
        };
        // Switches used when taking automated screenshots.
        let args: Vec<String> = std::env::args_os().map(|a| a.to_string_lossy().into_owned()).collect();
        let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
        app.show_thumbs = args.iter().any(|a| a == "--thumbs");
        if let Some(q) = arg("--find") {
            app.search.open = true;
            app.search.query = q;
        }
        if let Some(steps) = arg("--do") {
            app.script = steps.split('|').rev().map(str::to_string).collect();
        }
        if let Some(p) = arg("--page").and_then(|p| p.parse::<usize>().ok()) {
            app.jump = Some(p.saturating_sub(1));
        }
        if let Some(file) = file {
            app.load(&cc.egui_ctx, &file);
        }
        app
    }

    fn pool(ctx: &egui::Context, bytes: &Arc<Vec<u8>>, password: &str) -> Pool {
        let ctx = ctx.clone();
        Pool::new(bytes.clone(), password.to_string(), move || ctx.request_repaint())
    }

    fn load(&mut self, ctx: &egui::Context, path: &Path) {
        match std::fs::read(path) {
            Ok(data) => {
                let bytes = Arc::new(data);
                self.doc = Some(Doc {
                    path: Some(path.to_path_buf()),
                    saved: bytes.clone(),
                    pool: Self::pool(ctx, &bytes, ""),
                    bytes,
                    password: String::new(),
                    sizes: Vec::new(),
                    loaded: false,
                    tex: HashMap::new(),
                    thumbs: HashMap::new(),
                    texts: HashMap::new(),
                    undo: Vec::new(),
                    redo: Vec::new(),
                });
                self.init_zoom = true;
                self.scroll = Vec2::ZERO;
                self.set_scroll = Some(Vec2::ZERO);
                self.cur_page = 0;
                self.sel = None;
                self.draft = None;
                self.tool = Tool::Select;
                self.reset_search();
                self.status.clear();
            }
            Err(e) => self.error = Some(format!("Cannot open {}\n{e}", path.display())),
        }
    }

    fn reset_search(&mut self) {
        self.search.active.clear();
        self.search.hits.clear();
        self.search.scanned.clear();
        self.search.cur = None;
    }

    /// Swap in a new version of the document (after an edit, undo or redo).
    fn replace(&mut self, ctx: &egui::Context, snap: Snap, jump: Option<usize>) {
        let Some(doc) = self.doc.as_mut() else { return };
        doc.pool = Self::pool(ctx, &snap.0, &snap.1);
        doc.bytes = snap.0;
        doc.password = snap.1;
        doc.loaded = false;
        self.sel = None;
        self.draft = None;
        self.reset_search();
        match jump {
            Some(p) => self.jump = Some(p),
            None => self.set_scroll = Some(self.scroll),
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let mut failed = None;
        if let Some(doc) = self.doc.as_mut() {
            while let Ok(out) = doc.pool.rx.try_recv() {
                match out {
                    Out::Opened(sizes) => {
                        doc.sizes = sizes;
                        doc.loaded = true;
                        doc.tex.clear();
                        doc.thumbs.clear();
                        doc.texts.clear();
                        self.cur_page = self.cur_page.min(doc.sizes.len().saturating_sub(1));
                    }
                    Out::Failed(e) => failed = Some(e),
                    Out::Image { page, width, w, h, rgba } => {
                        if !doc.loaded || rgba.len() != w * h * 4 {
                            continue;
                        }
                        let img = egui::ColorImage::from_rgba_premultiplied([w, h], &rgba);
                        if width == THUMB_PX {
                            let tex = ctx.load_texture(format!("t{page}"), img, TextureOptions::LINEAR);
                            doc.thumbs.insert(page, tex);
                        } else {
                            let tex = ctx.load_texture(format!("p{page}"), img, TextureOptions::LINEAR);
                            doc.tex.insert(page, PageTex { tex, width, used: self.frame, bytes: w * h * 4 });
                            let mut total: usize = doc.tex.values().map(|t| t.bytes).sum();
                            while total > TEX_BUDGET && doc.tex.len() > 1 {
                                let Some(old) = doc
                                    .tex
                                    .iter()
                                    .filter(|(p, _)| **p != page)
                                    .min_by_key(|(_, t)| t.used)
                                    .map(|(p, _)| *p)
                                else {
                                    break;
                                };
                                total -= doc.tex.remove(&old).map(|t| t.bytes).unwrap_or(0);
                            }
                        }
                    }
                    Out::Text { page, text } => {
                        if doc.loaded {
                            doc.texts.insert(page, Arc::new(text));
                        }
                    }
                }
            }
            if doc.thumbs.len() > 400 {
                let cur = self.cur_page;
                doc.thumbs.retain(|p, _| p.abs_diff(cur) < 150);
            }
        }
        match failed {
            Some(OpenError::NeedPassword) => {
                let wrong = self.doc.as_ref().is_some_and(|d| !d.password.is_empty());
                self.password = Some((String::new(), wrong));
            }
            Some(OpenError::Invalid) => {
                let doc = self.doc.as_mut().unwrap();
                if let Some(prev) = doc.undo.pop() {
                    // An edit produced something unreadable: go back to the last good version.
                    self.replace(ctx, prev, None);
                    self.error = Some("The edit could not be applied to this PDF.".into());
                } else {
                    let name = doc.name();
                    self.doc = None;
                    self.error = Some(format!("{name} is not a PDF file that can be read."));
                }
            }
            None => {}
        }

        if let Some(rx) = &self.edit_rx {
            if let Ok(done) = rx.try_recv() {
                self.edit_rx = None;
                match done {
                    EditDone::Replace(Ok(bytes), jump, label) => {
                        if let Some(doc) = self.doc.as_mut() {
                            doc.undo.push((doc.bytes.clone(), doc.password.clone()));
                            if doc.undo.len() > 30 {
                                doc.undo.remove(0);
                            }
                            doc.redo.clear();
                        }
                        self.replace(ctx, (Arc::new(bytes), String::new()), jump);
                        self.status = label.to_string();
                    }
                    EditDone::Replace(Err(e), _, _) => {
                        self.status.clear();
                        self.error = Some(e);
                    }
                    EditDone::Export(Ok(()), path) => self.status = format!("Saved {}", path.display()),
                    EditDone::Export(Err(e), _) => {
                        self.status.clear();
                        self.error = Some(e);
                    }
                }
            }
        }

        if let Some(rx) = &self.print_rx {
            let mut end = false;
            while let Ok(p) = rx.try_recv() {
                match p {
                    print::Progress::Page(i, n) => self.status = format!("Printing page {i} of {n}"),
                    print::Progress::Done => {
                        self.status = "Sent to the printer".into();
                        end = true;
                    }
                    print::Progress::Failed(e) => {
                        self.status.clear();
                        self.error = Some(e);
                        end = true;
                    }
                }
            }
            if end {
                self.print_rx = None;
            }
        }

        self.scan_search();
    }

    /// Search pages whose text has arrived since the last frame.
    fn scan_search(&mut self) {
        let Some(doc) = self.doc.as_ref() else { return };
        let s = &mut self.search;
        let q = if s.open { s.query.trim().to_string() } else { String::new() };
        if q != s.active {
            s.active = q;
            s.hits.clear();
            s.scanned.clear();
            s.cur = None;
            s.seek = true;
        }
        if s.active.is_empty() {
            return;
        }
        let needle = lower_chars(&s.active);
        let keep = s.cur.and_then(|i| s.hits.get(i).copied());
        let mut added = false;
        for (page, text) in &doc.texts {
            if s.scanned.insert(*page) {
                for (a, b) in text.find(&needle) {
                    s.hits.push(Hit { page: *page, a, b });
                    added = true;
                }
            }
        }
        if added {
            s.hits.sort_by_key(|h| (h.page, h.a));
            s.cur = keep.and_then(|k| s.hits.iter().position(|h| *h == k));
        }
        if s.seek && !s.hits.is_empty() {
            // Wait until every page before the first hit has been looked at.
            let cur = self.cur_page;
            let first = s.hits.iter().position(|h| h.page >= cur).unwrap_or(0);
            let upto = s.hits[first].page;
            let from = if s.hits[first].page >= cur { cur } else { 0 };
            if (from..=upto).all(|p| s.scanned.contains(&p)) {
                s.seek = false;
                s.cur = Some(first);
                let hit = s.hits[first];
                self.show_hit(hit);
            }
        }
    }

    fn show_hit(&mut self, hit: Hit) {
        let Some(doc) = self.doc.as_ref() else { return };
        let lay = layout(&doc.sizes, self.zoom);
        let Some(top) = lay.tops.get(hit.page) else { return };
        let y = doc
            .texts
            .get(&hit.page)
            .and_then(|t| t.rects(hit.a, hit.b).first().map(|r| r[1]))
            .unwrap_or(0.0);
        let target = top + y * lay.ppt;
        // Leave the view alone if the hit is already comfortably on screen.
        if target < self.scroll.y + 20.0 || target > self.scroll.y + self.view_size.y - 80.0 {
            self.set_scroll = Some(vec2(self.scroll.x, (target - self.view_size.y * 0.3).max(0.0)));
        }
    }

    fn set_zoom(&mut self, z: f32) {
        let z = z.clamp(0.1, 6.0);
        if let Some(doc) = &self.doc {
            if !doc.sizes.is_empty() {
                let old = layout(&doc.sizes, self.zoom);
                let new = layout(&doc.sizes, z);
                let y = self.set_scroll.map_or(self.scroll.y, |s| s.y);
                let i = old.page_at(y);
                let frac = (y - old.tops[i]) / (doc.sizes[i].1 * old.ppt).max(1.0);
                let ny = new.tops[i] + frac * doc.sizes[i].1 * new.ppt;
                let cx = (self.scroll.x + self.view_size.x * 0.5) / old.w.max(1.0);
                let nx = cx * new.w - self.view_size.x * 0.5;
                self.set_scroll = Some(vec2(nx.max(0.0), ny.max(0.0)));
            }
        }
        self.zoom = z;
    }

    fn fit_zoom(&self, fit: Fit) -> f32 {
        let Some(doc) = &self.doc else { return self.zoom };
        if doc.sizes.is_empty() {
            return self.zoom;
        }
        let bar = 16.0;
        let avail_w = (self.view_size.x - 2.0 * MARGIN - bar).max(50.0);
        match fit {
            Fit::None => self.zoom,
            Fit::Width => {
                let max_w = doc.sizes.iter().map(|s| s.0).fold(1.0, f32::max);
                avail_w / (max_w * PT_TO_PX)
            }
            Fit::Page => {
                let (w, h) = doc.sizes[self.cur_page.min(doc.sizes.len() - 1)];
                let avail_h = (self.view_size.y - 2.0 * GAP).max(50.0);
                (avail_w / (w * PT_TO_PX)).min(avail_h / (h * PT_TO_PX))
            }
        }
        .clamp(0.1, 6.0)
    }

    fn busy(&self) -> bool {
        self.edit_rx.is_some()
    }

    fn run_edit(&mut self, ctx: &egui::Context, op: Op, jump: Option<usize>, label: &'static str) {
        let Some(doc) = &self.doc else { return };
        if self.busy() || !doc.loaded {
            return;
        }
        let (bytes, password) = (doc.bytes.clone(), doc.password.clone());
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = std::panic::catch_unwind(move || edit::apply(&bytes, &password, op))
                .unwrap_or_else(|_| Err("This PDF could not be edited.".into()));
            let _ = tx.send(EditDone::Replace(res, jump, label));
            ctx.request_repaint();
        });
        self.edit_rx = Some(rx);
        self.status = "Working...".into();
    }

    fn save_to(&mut self, path: &Path) -> bool {
        let Some(doc) = self.doc.as_mut() else { return false };
        let tmp = path.with_extension("pdf.saving");
        let res = std::fs::write(&tmp, &**doc.bytes).and_then(|_| std::fs::rename(&tmp, path));
        match res {
            Ok(()) => {
                doc.saved = doc.bytes.clone();
                doc.path = Some(path.to_path_buf());
                self.status = format!("Saved {}", path.display());
                true
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                self.error = Some(format!("Cannot save {}\n{e}", path.display()));
                false
            }
        }
    }

    fn save_as_dialog(&mut self, suffix: &str) -> Option<PathBuf> {
        let doc = self.doc.as_ref()?;
        let mut dlg = rfd::FileDialog::new().add_filter("PDF", &["pdf"]);
        if let Some(p) = &doc.path {
            if let Some(dir) = p.parent() {
                dlg = dlg.set_directory(dir);
            }
            let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            dlg = dlg.set_file_name(format!("{stem}{suffix}.pdf"));
        }
        let mut path = dlg.save_file()?;
        if path.extension().is_none() {
            path.set_extension("pdf");
        }
        Some(path)
    }

    fn save(&mut self, ask: bool) -> bool {
        let existing = self.doc.as_ref().and_then(|d| d.path.clone());
        let path = match existing {
            Some(p) if !ask => Some(p),
            _ => self.save_as_dialog(""),
        };
        match path {
            Some(p) => self.save_to(&p),
            None => false,
        }
    }

    fn selection_text(&self) -> Option<String> {
        let sel = self.sel.as_ref()?;
        let text = self.doc.as_ref()?.texts.get(&sel.page)?;
        let (a, b) = (sel.anchor.min(sel.head), sel.anchor.max(sel.head));
        let s = text.text_range(a, text.glyph_end(b));
        (!s.trim().is_empty()).then_some(s)
    }

    fn act(&mut self, ctx: &egui::Context, act: Act) {
        let n = self.doc.as_ref().map_or(0, |d| d.sizes.len());
        let cur = self.cur_page.min(n.saturating_sub(1));
        match act {
            Act::Open => {
                let mut dlg = rfd::FileDialog::new().add_filter("PDF", &["pdf"]);
                if let Some(dir) = self.doc.as_ref().and_then(|d| d.path.as_ref()).and_then(|p| p.parent()) {
                    dlg = dlg.set_directory(dir);
                }
                if let Some(path) = dlg.pick_file() {
                    self.act(ctx, Act::OpenPath(path));
                }
            }
            Act::OpenPath(path) => {
                if self.doc.is_none() {
                    self.load(ctx, &path);
                } else if let Ok(exe) = std::env::current_exe() {
                    // A second document gets its own window.
                    let _ = std::process::Command::new(exe).arg(path).spawn();
                }
            }
            Act::Save => {
                self.save(false);
            }
            Act::SaveAs => {
                self.save(true);
            }
            Act::Print => {
                let Some(doc) = &self.doc else { return };
                if !doc.loaded || self.print_rx.is_some() {
                    return;
                }
                if let Some(job) = print::choose_printer(n, cur) {
                    let (tx, rx) = channel();
                    let ctx2 = ctx.clone();
                    print::start(job, doc.bytes.clone(), doc.password.clone(), doc.name(), tx, move || {
                        ctx2.request_repaint()
                    });
                    self.print_rx = Some(rx);
                    self.status = "Printing...".into();
                }
            }
            Act::Undo => {
                if self.busy() {
                    return;
                }
                let Some(doc) = self.doc.as_mut() else { return };
                if let Some(prev) = doc.undo.pop() {
                    doc.redo.push((doc.bytes.clone(), doc.password.clone()));
                    self.replace(ctx, prev, None);
                    self.status = "Undone".into();
                }
            }
            Act::Redo => {
                if self.busy() {
                    return;
                }
                let Some(doc) = self.doc.as_mut() else { return };
                if let Some(next) = doc.redo.pop() {
                    doc.undo.push((doc.bytes.clone(), doc.password.clone()));
                    self.replace(ctx, next, None);
                    self.status = "Redone".into();
                }
            }
            Act::Copy => {
                if let Some(s) = self.selection_text() {
                    ctx.copy_text(s);
                    self.status = "Copied".into();
                }
            }
            Act::SelectAll => {
                if let Some(text) = self.doc.as_ref().and_then(|d| d.texts.get(&cur)) {
                    if !text.is_empty() {
                        self.sel = Some(Sel { page: cur, anchor: 0, head: text.len() - 1 });
                    }
                }
            }
            Act::Find => {
                self.search.open = true;
                self.search.focus = true;
            }
            Act::FindNext(forward) => {
                let s = &mut self.search;
                if s.hits.is_empty() {
                    return;
                }
                let len = s.hits.len();
                let i = match (s.cur, forward) {
                    (Some(i), true) => (i + 1) % len,
                    (Some(i), false) => (i + len - 1) % len,
                    (None, _) => 0,
                };
                s.cur = Some(i);
                let hit = s.hits[i];
                self.show_hit(hit);
            }
            Act::ZoomStep(up) => {
                let z = self.zoom;
                let next = if up {
                    ZOOMS.iter().copied().find(|v| *v > z * 1.01).unwrap_or(6.0)
                } else {
                    ZOOMS.iter().rev().copied().find(|v| *v < z * 0.99).unwrap_or(0.25)
                };
                self.fit = Fit::None;
                self.set_zoom(next);
            }
            Act::Zoom(z) => {
                self.fit = Fit::None;
                self.set_zoom(z);
            }
            Act::Fit(fit) => {
                self.fit = fit;
                let z = self.fit_zoom(fit);
                self.set_zoom(z);
                if fit == Fit::Page && n > 0 {
                    // A whole page fits now, so line it up with the window.
                    self.jump = Some(cur);
                }
            }
            Act::Jump(p) => {
                if n > 0 {
                    self.jump = Some(p.min(n - 1));
                }
            }
            Act::ScrollBy(d) => {
                let base = self.set_scroll.unwrap_or(self.scroll);
                self.set_scroll = Some((base + d).max(Vec2::ZERO));
            }
            Act::ScrollEnd(end) => {
                self.set_scroll = Some(vec2(self.scroll.x, if end { f32::MAX / 4.0 } else { 0.0 }));
            }
            Act::Rotate(deg, all) => {
                let pages = if all { (0..n).collect() } else { vec![cur] };
                self.run_edit(ctx, Op::Rotate { pages, degrees: deg }, Some(cur), "Rotated");
            }
            Act::Delete => {
                if n > 1 {
                    self.run_edit(ctx, Op::Delete { pages: vec![cur] }, Some(cur.min(n - 2)), "Page deleted");
                }
            }
            Act::MovePage(down) => {
                let to = if down { cur + 1 } else { cur.wrapping_sub(1) };
                if to < n {
                    self.run_edit(ctx, Op::Move { from: cur, to }, Some(to), "Page moved");
                }
            }
            Act::InsertPdf(at_end) => {
                if n == 0 {
                    return;
                }
                let Some(path) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_file() else { return };
                match std::fs::read(&path) {
                    Ok(other) => {
                        let at = if at_end { n } else { cur + 1 };
                        self.run_edit(ctx, Op::Insert { at, other }, Some(at), "Pages inserted");
                    }
                    Err(e) => self.error = Some(format!("Cannot open {}\n{e}", path.display())),
                }
            }
            Act::ExtractDialog => {
                if n > 0 {
                    self.extract = Some(format!("{}", cur + 1));
                }
            }
            Act::Extract(pages) => {
                let Some(path) = self.save_as_dialog("-pages") else { return };
                let Some(doc) = &self.doc else { return };
                if self.busy() {
                    return;
                }
                let (bytes, password) = (doc.bytes.clone(), doc.password.clone());
                let (tx, rx) = channel();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let res = std::panic::catch_unwind(|| edit::apply(&bytes, &password, Op::Keep { pages }))
                        .unwrap_or_else(|_| Err("These pages could not be extracted.".into()))
                        .and_then(|out| std::fs::write(&path, out).map_err(|e| format!("Cannot save {}\n{e}", path.display())));
                    let _ = tx.send(EditDone::Export(res, path));
                    ctx.request_repaint();
                });
                self.edit_rx = Some(rx);
                self.status = "Working...".into();
            }
            Act::ToolAddText => {
                self.tool = if self.tool == Tool::AddText { Tool::Select } else { Tool::AddText };
                self.draft = None;
                if self.tool == Tool::AddText {
                    self.status = "Click on the page where the text should start".into();
                }
            }
            Act::AddText => {
                if let Some(d) = self.draft.take() {
                    self.tool = Tool::Select;
                    if !d.text.trim().is_empty() {
                        let op = Op::AddText {
                            page: d.page,
                            x: d.x,
                            y: d.y,
                            text: d.text,
                            size: d.size,
                            color: TEXT_COLORS[d.color].1,
                        };
                        self.run_edit(ctx, op, None, "Text added");
                    }
                }
            }
            Act::ToggleThumbs => {
                self.show_thumbs = !self.show_thumbs;
                self.thumb_follow = usize::MAX;
            }
            Act::MakeDefault => match std::env::current_exe().map_err(|e| e.to_string()).and_then(|exe| register::register(&exe)) {
                Ok(()) => {
                    register::open_settings();
                    self.status = "In the Windows page that opened, choose ProtoPDF for .pdf".into();
                }
                Err(e) => self.error = Some(format!("Could not register with Windows\n{e}")),
            },
            Act::Password(pw) => {
                self.password = None;
                match (pw, self.doc.as_mut()) {
                    (Some(pw), Some(doc)) => {
                        doc.pool = Self::pool(ctx, &doc.bytes, &pw);
                        doc.password = pw;
                    }
                    _ => self.doc = None,
                }
            }
            Act::CloseAnswer(answer) => {
                self.confirm_close = false;
                // 0 = save, 1 = discard, 2 = stay.
                if answer == 1 || (answer == 0 && self.save(false)) {
                    self.force_close = true;
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
            Act::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        let typing = ctx.egui_wants_keyboard_input();
        let modal = self.password.is_some() || self.error.is_some() || self.confirm_close || self.extract.is_some();
        let hit = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));
        let ctrl = Modifiers::COMMAND;
        let ctrl_shift = Modifiers::COMMAND | Modifiers::SHIFT;

        if ctx.input(|i| i.viewport().close_requested()) && !self.force_close {
            if self.doc.as_ref().is_some_and(|d| d.dirty()) {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                self.confirm_close = true;
            }
        }
        for f in ctx.input(|i| i.raw.dropped_files.clone()) {
            let p = f.path();
            if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")) {
                acts.push(Act::OpenPath(p.to_path_buf()));
            }
        }
        if modal {
            return;
        }

        if hit(ctrl_shift, Key::S) {
            acts.push(Act::SaveAs);
        }
        if hit(ctrl, Key::O) {
            acts.push(Act::Open);
        }
        if hit(ctrl, Key::S) {
            acts.push(Act::Save);
        }
        if hit(ctrl, Key::P) {
            acts.push(Act::Print);
        }
        if hit(ctrl, Key::F) {
            acts.push(Act::Find);
        }
        if hit(ctrl, Key::Plus) || hit(ctrl, Key::Equals) {
            acts.push(Act::ZoomStep(true));
        }
        if hit(ctrl, Key::Minus) {
            acts.push(Act::ZoomStep(false));
        }
        if hit(ctrl, Key::Num0) {
            acts.push(Act::Fit(Fit::Page));
        }
        if hit(ctrl, Key::Num1) {
            acts.push(Act::Zoom(1.0));
        }
        if hit(ctrl, Key::Num2) {
            acts.push(Act::Fit(Fit::Width));
        }
        if hit(Modifiers::NONE, Key::F3) {
            acts.push(Act::FindNext(true));
        }
        if hit(Modifiers::SHIFT, Key::F3) {
            acts.push(Act::FindNext(false));
        }
        if hit(Modifiers::NONE, Key::Escape) {
            if self.draft.is_some() || self.tool == Tool::AddText {
                self.draft = None;
                self.tool = Tool::Select;
            } else if self.search.open {
                self.search.open = false;
            } else {
                self.sel = None;
            }
        }
        let zoom = ctx.input(|i| i.zoom_delta());
        if (zoom - 1.0).abs() > 0.001 && self.doc.is_some() {
            acts.push(Act::Zoom(self.zoom * zoom));
        }

        if typing || self.draft.is_some() {
            return;
        }
        if ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
            acts.push(Act::Copy);
        }
        if hit(ctrl_shift, Key::Z) || hit(ctrl, Key::Y) {
            acts.push(Act::Redo);
        }
        if hit(ctrl, Key::Z) {
            acts.push(Act::Undo);
        }
        if hit(ctrl, Key::A) {
            acts.push(Act::SelectAll);
        }
        if hit(ctrl, Key::R) {
            acts.push(Act::Rotate(90, false));
        }
        let page = self.view_size.y * 0.9;
        if hit(Modifiers::NONE, Key::PageDown) || hit(Modifiers::NONE, Key::Space) {
            acts.push(Act::ScrollBy(vec2(0.0, page)));
        }
        if hit(Modifiers::NONE, Key::PageUp) || hit(Modifiers::SHIFT, Key::Space) {
            acts.push(Act::ScrollBy(vec2(0.0, -page)));
        }
        if hit(Modifiers::NONE, Key::ArrowDown) {
            acts.push(Act::ScrollBy(vec2(0.0, 60.0)));
        }
        if hit(Modifiers::NONE, Key::ArrowUp) {
            acts.push(Act::ScrollBy(vec2(0.0, -60.0)));
        }
        if hit(Modifiers::NONE, Key::ArrowRight) {
            acts.push(Act::Jump(self.cur_page + 1));
        }
        if hit(Modifiers::NONE, Key::ArrowLeft) {
            acts.push(Act::Jump(self.cur_page.saturating_sub(1)));
        }
        if hit(Modifiers::NONE, Key::Home) || hit(ctrl, Key::Home) {
            acts.push(Act::ScrollEnd(false));
        }
        if hit(Modifiers::NONE, Key::End) || hit(ctrl, Key::End) {
            acts.push(Act::ScrollEnd(true));
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let has = self.doc.as_ref().is_some_and(|d| d.loaded);
        let n = self.doc.as_ref().map_or(0, |d| d.sizes.len());
        let dirty = self.doc.as_ref().is_some_and(|d| d.dirty());
        let can_undo = self.doc.as_ref().is_some_and(|d| !d.undo.is_empty());
        let can_redo = self.doc.as_ref().is_some_and(|d| !d.redo.is_empty());
        let edit_ok = has && !self.busy();
        let item = |ui: &mut egui::Ui, on: bool, label: &str, keys: &str| -> bool {
            let mut b = egui::Button::new(label);
            if !keys.is_empty() {
                b = b.shortcut_text(keys);
            }
            let clicked = ui.add_enabled(on, b).clicked();
            if clicked {
                ui.close();
            }
            clicked
        };

        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if item(ui, true, "Open...", "Ctrl+O") {
                        acts.push(Act::Open);
                    }
                    if item(ui, has && dirty, "Save", "Ctrl+S") {
                        acts.push(Act::Save);
                    }
                    if item(ui, has, "Save as...", "Ctrl+Shift+S") {
                        acts.push(Act::SaveAs);
                    }
                    ui.separator();
                    if item(ui, has, "Print...", "Ctrl+P") {
                        acts.push(Act::Print);
                    }
                    ui.separator();
                    if register::is_default() {
                        item(ui, false, "ProtoPDF is the default PDF app", "");
                    } else if item(ui, true, "Make ProtoPDF the default PDF app...", "") {
                        acts.push(Act::MakeDefault);
                    }
                    ui.separator();
                    if item(ui, true, "Exit", "") {
                        acts.push(Act::Quit);
                    }
                });
                ui.menu_button("Edit", |ui| {
                    if item(ui, can_undo && edit_ok, "Undo", "Ctrl+Z") {
                        acts.push(Act::Undo);
                    }
                    if item(ui, can_redo && edit_ok, "Redo", "Ctrl+Y") {
                        acts.push(Act::Redo);
                    }
                    ui.separator();
                    if item(ui, self.sel.is_some(), "Copy", "Ctrl+C") {
                        acts.push(Act::Copy);
                    }
                    if item(ui, has, "Select all text on this page", "Ctrl+A") {
                        acts.push(Act::SelectAll);
                    }
                    if item(ui, has, "Find...", "Ctrl+F") {
                        acts.push(Act::Find);
                    }
                    ui.separator();
                    if item(ui, edit_ok, "Add text...", "") {
                        acts.push(Act::ToolAddText);
                    }
                });
                ui.menu_button("Page", |ui| {
                    if item(ui, edit_ok, "Rotate right", "Ctrl+R") {
                        acts.push(Act::Rotate(90, false));
                    }
                    if item(ui, edit_ok, "Rotate left", "") {
                        acts.push(Act::Rotate(-90, false));
                    }
                    if item(ui, edit_ok, "Rotate all pages right", "") {
                        acts.push(Act::Rotate(90, true));
                    }
                    ui.separator();
                    if item(ui, edit_ok && self.cur_page > 0, "Move page up", "") {
                        acts.push(Act::MovePage(false));
                    }
                    if item(ui, edit_ok && self.cur_page + 1 < n, "Move page down", "") {
                        acts.push(Act::MovePage(true));
                    }
                    if item(ui, edit_ok && n > 1, "Delete page", "") {
                        acts.push(Act::Delete);
                    }
                    ui.separator();
                    if item(ui, edit_ok, "Insert PDF after this page...", "") {
                        acts.push(Act::InsertPdf(false));
                    }
                    if item(ui, edit_ok, "Add PDF at the end...", "") {
                        acts.push(Act::InsertPdf(true));
                    }
                    if item(ui, edit_ok, "Save pages as a new PDF...", "") {
                        acts.push(Act::ExtractDialog);
                    }
                });
                ui.menu_button("View", |ui| {
                    if item(ui, has, "Zoom in", "Ctrl++") {
                        acts.push(Act::ZoomStep(true));
                    }
                    if item(ui, has, "Zoom out", "Ctrl+-") {
                        acts.push(Act::ZoomStep(false));
                    }
                    if item(ui, has, "Actual size", "Ctrl+1") {
                        acts.push(Act::Zoom(1.0));
                    }
                    if item(ui, has, "Fit width", "Ctrl+2") {
                        acts.push(Act::Fit(Fit::Width));
                    }
                    if item(ui, has, "Fit page", "Ctrl+0") {
                        acts.push(Act::Fit(Fit::Page));
                    }
                    ui.separator();
                    if item(ui, true, if self.show_thumbs { "Hide page list" } else { "Show page list" }, "") {
                        acts.push(Act::ToggleThumbs);
                    }
                });
            });
        });

        egui::Panel::top("tools").show(ui, |ui| {
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if ui.button("Open").on_hover_text("Open a PDF (Ctrl+O)").clicked() {
                    acts.push(Act::Open);
                }
                if ui.add_enabled(has && dirty, egui::Button::new("Save")).on_hover_text("Save (Ctrl+S)").clicked() {
                    acts.push(Act::Save);
                }
                if printer_button(ui, has).on_hover_text("Print (Ctrl+P)").clicked() {
                    acts.push(Act::Print);
                }
                ui.separator();
                if ui.selectable_label(self.show_thumbs, "Pages").on_hover_text("Show or hide the page list").clicked() {
                    acts.push(Act::ToggleThumbs);
                }
                ui.separator();

                if ui.add_enabled(has && self.cur_page > 0, egui::Button::new("<")).on_hover_text("Previous page").clicked() {
                    acts.push(Act::Jump(self.cur_page.saturating_sub(1)));
                }
                let field = ui.add_enabled(
                    has,
                    egui::TextEdit::singleline(&mut self.page_field).desired_width(34.0).align(Align2::CENTER_CENTER),
                );
                if field.lost_focus() {
                    if let Ok(p) = self.page_field.trim().parse::<usize>() {
                        if p >= 1 && p - 1 != self.cur_page {
                            acts.push(Act::Jump(p - 1));
                        }
                    }
                }
                if !field.has_focus() {
                    self.page_field = if n > 0 { (self.cur_page + 1).to_string() } else { String::new() };
                }
                ui.label(format!("/ {n}"));
                if ui.add_enabled(has && self.cur_page + 1 < n, egui::Button::new(">")).on_hover_text("Next page").clicked() {
                    acts.push(Act::Jump(self.cur_page + 1));
                }
                ui.separator();

                if ui.add_enabled(has, egui::Button::new("-")).on_hover_text("Zoom out (Ctrl+-)").clicked() {
                    acts.push(Act::ZoomStep(false));
                }
                ui.add_enabled_ui(has, |ui| {
                    ui.menu_button(format!("{:.0}%", self.zoom * 100.0), |ui| {
                        for z in ZOOMS {
                            if ui.button(format!("{:.0}%", z * 100.0)).clicked() {
                                acts.push(Act::Zoom(z));
                                ui.close();
                            }
                        }
                    });
                });
                if ui.add_enabled(has, egui::Button::new("+")).on_hover_text("Zoom in (Ctrl++)").clicked() {
                    acts.push(Act::ZoomStep(true));
                }
                if ui.add_enabled(has, egui::Button::selectable(self.fit == Fit::Width, "Fit width")).clicked() {
                    acts.push(Act::Fit(Fit::Width));
                }
                if ui.add_enabled(has, egui::Button::selectable(self.fit == Fit::Page, "Fit page")).clicked() {
                    acts.push(Act::Fit(Fit::Page));
                }
                ui.separator();

                if ui.add_enabled(edit_ok, egui::Button::new("Rotate")).on_hover_text("Rotate this page right (Ctrl+R)").clicked() {
                    acts.push(Act::Rotate(90, false));
                }
                if ui
                    .add_enabled(edit_ok, egui::Button::selectable(self.tool == Tool::AddText, "Add text"))
                    .on_hover_text("Write text on the page")
                    .clicked()
                {
                    acts.push(Act::ToolAddText);
                }
                if ui.add_enabled(can_undo && edit_ok, egui::Button::new("Undo")).on_hover_text("Undo (Ctrl+Z)").clicked() {
                    acts.push(Act::Undo);
                }
                ui.separator();
                if ui.add_enabled(has, egui::Button::selectable(self.search.open, "Find")).on_hover_text("Find text (Ctrl+F)").clicked() {
                    if self.search.open {
                        self.search.open = false;
                    } else {
                        acts.push(Act::Find);
                    }
                }
            });
            ui.add_space(3.0);
        });

        if self.search.open && has {
            egui::Panel::top("find").show(ui, |ui| {
                ui.add_space(3.0);
                ui.horizontal(|ui| {
                    ui.label("Find:");
                    let r = ui.add(egui::TextEdit::singleline(&mut self.search.query).desired_width(240.0).hint_text("text to find"));
                    if std::mem::take(&mut self.search.focus) {
                        r.request_focus();
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        acts.push(Act::FindNext(!ui.input(|i| i.modifiers.shift)));
                        r.request_focus();
                    }
                    let any = !self.search.hits.is_empty();
                    if ui.add_enabled(any, egui::Button::new("Previous")).clicked() {
                        acts.push(Act::FindNext(false));
                    }
                    if ui.add_enabled(any, egui::Button::new("Next")).clicked() {
                        acts.push(Act::FindNext(true));
                    }
                    let s = &self.search;
                    if !s.active.is_empty() {
                        let pending = n.saturating_sub(s.scanned.len());
                        let msg = match (s.hits.len(), s.cur, pending) {
                            (0, _, 0) => "Not found".to_string(),
                            (0, _, _) => "Searching...".to_string(),
                            (len, Some(i), 0) => format!("{} of {len}", i + 1),
                            (len, Some(i), _) => format!("{} of {len}, still searching...", i + 1),
                            (len, None, _) => format!("{len} found"),
                        };
                        ui.label(msg);
                    }
                    ui.with_layout(UiLayout::right_to_left(Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            self.search.open = false;
                        }
                    });
                });
                ui.add_space(3.0);
            });
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                if self.busy() || self.print_rx.is_some() {
                    ui.spinner();
                }
                ui.label(&self.status);
                ui.with_layout(UiLayout::right_to_left(Align::Center), |ui| {
                    if let Some(doc) = &self.doc {
                        if let Some((w, h)) = doc.sizes.get(self.cur_page) {
                            ui.label(format!("{:.0} x {:.0} mm", w * 25.4 / 72.0, h * 25.4 / 72.0));
                            ui.separator();
                        }
                        if doc.dirty() {
                            ui.label("Not saved");
                        }
                    }
                });
            });
        });
    }

    fn thumbs_panel(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let Some(doc) = self.doc.as_mut() else { return };
        let cur = self.cur_page;
        let jobs = &mut self.jobs_thumb;
        let item_h = |s: &(f32, f32)| THUMB_W * (s.1 / s.0.max(1.0)).clamp(0.2, 3.0) + 26.0;

        // Keep the current page in sight when it changes.
        if self.thumb_follow != cur && doc.loaded {
            self.thumb_follow = cur;
            let top: f32 = doc.sizes[..cur.min(doc.sizes.len())].iter().map(item_h).sum();
            let h = doc.sizes.get(cur).map(item_h).unwrap_or(0.0);
            let (v_top, v_h) = self.thumb_view;
            if top < v_top || top + h > v_top + v_h {
                self.thumb_scroll = Some((top - 30.0).max(0.0));
            }
        }

        egui::Panel::left("thumbs").resizable(false).exact_size(THUMB_PANEL).show(ui, |ui| {
            let mut area = ScrollArea::vertical().auto_shrink(false).id_salt("thumbs");
            if let Some(y) = self.thumb_scroll.take() {
                area = area.vertical_scroll_offset(y);
            }
            let out = area.show_viewport(ui, |ui, viewport| {
                let total: f32 = doc.sizes.iter().map(item_h).sum();
                let width = ui.available_width();
                let (rect, _) = ui.allocate_exact_size(vec2(width, total.max(viewport.height())), Sense::hover());
                let painter = ui.painter();
                let accent = ui.visuals().selection.bg_fill;
                let text_color = ui.visuals().text_color();
                let mut y = 0.0;
                for (i, s) in doc.sizes.iter().enumerate() {
                    let h = item_h(s);
                    if y + h >= viewport.top() && y <= viewport.bottom() {
                        let th = h - 26.0;
                        let r = Rect::from_min_size(
                            pos2(rect.left() + (width - THUMB_W) * 0.5, rect.top() + y + 6.0),
                            vec2(THUMB_W, th),
                        );
                        let resp = ui.interact(r.expand(4.0), ui.scope_id().with(("thumb", i)), Sense::click());
                        if resp.clicked() {
                            acts.push(Act::Jump(i));
                        }
                        if i == cur || resp.hovered() {
                            let a = if i == cur { accent } else { accent.gamma_multiply(0.45) };
                            painter.rect_filled(r.expand(4.0), 3.0, a);
                        }
                        painter.rect_filled(r, 0.0, Color32::WHITE);
                        match doc.thumbs.get(&i) {
                            Some(tex) => {
                                painter.image(tex.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                            }
                            None if doc.loaded => jobs.push(Job { page: i, kind: Kind::Render(THUMB_PX) }),
                            None => {}
                        }
                        painter.rect_stroke(r, 0.0, Stroke::new(1.0, Color32::from_gray(150)), StrokeKind::Outside);
                        painter.text(
                            pos2(r.center().x, r.bottom() + 3.0),
                            Align2::CENTER_TOP,
                            (i + 1).to_string(),
                            FontId::proportional(11.0),
                            text_color,
                        );
                    }
                    y += h;
                }
            });
            self.thumb_view = (out.state.clamped_offset().y, out.inner_rect.height());
        });
    }

    fn welcome(ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.35);
            ui.heading("ProtoPDF");
            ui.add_space(10.0);
            if ui.add(egui::Button::new("Open a PDF").min_size(vec2(160.0, 36.0))).clicked() {
                acts.push(Act::Open);
            }
            ui.add_space(8.0);
            ui.label("or drop a file on this window");
        });
    }

    fn pages(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let dark = ui.visuals().dark_mode;
        let bg = if dark { Color32::from_gray(40) } else { Color32::from_gray(212) };
        let frame = egui::Frame::NONE.fill(bg);
        if self.doc.as_ref().is_none_or(|d| d.sizes.is_empty()) {
            let empty = self.doc.is_none();
            egui::CentralPanel::default().show(ui, |ui| {
                if empty {
                    Self::welcome(ui, acts);
                } else {
                    ui.centered_and_justified(|ui| ui.spinner());
                }
            });
            return;
        }

        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            self.view_size = ui.available_size();
            let loaded = self.doc.as_ref().is_some_and(|d| d.loaded);
            if loaded && self.init_zoom {
                self.init_zoom = false;
                self.zoom = self.fit_zoom(Fit::Width).min(1.25);
            }
            if loaded && self.fit != Fit::None {
                let z = self.fit_zoom(self.fit);
                if (z - self.zoom).abs() > 0.002 {
                    self.set_zoom(z);
                }
            }

            let ctx = ui.ctx().clone();
            let ppp = ctx.pixels_per_point();
            let max_side = ctx.input(|i| i.max_texture_side).min(8192) as f32;
            let frame_no = self.frame;
            let doc = self.doc.as_mut().unwrap();
            let lay = layout(&doc.sizes, self.zoom);
            let n = doc.sizes.len();

            if loaded {
                if let Some(p) = self.jump.take() {
                    let p = p.min(n - 1);
                    self.set_scroll = Some(vec2(self.scroll.x, (lay.tops[p] - GAP * 0.5).max(0.0)));
                }
            }
            let mut area = ScrollArea::both().auto_shrink(false).id_salt("pages");
            if let Some(s) = self.set_scroll.take() {
                area = area.scroll_offset(s.max(Vec2::ZERO));
            }

            let tool = self.tool;
            let sel = &mut self.sel;
            let draft = &mut self.draft;
            let search = &self.search;
            let jobs = &mut self.jobs_view;
            let mut cur_page = self.cur_page;
            let snap = |v: f32| (v * ppp).round() / ppp;

            let out = area.show_viewport(ui, |ui, viewport| {
                let total = vec2(lay.w.max(viewport.width()), lay.h.max(viewport.height()));
                let (rect, resp) = ui.allocate_exact_size(total, Sense::click_and_drag());
                let painter = ui.painter();
                let page_rect = |i: usize| {
                    let (w, h) = (doc.sizes[i].0 * lay.ppt, doc.sizes[i].1 * lay.ppt);
                    let min = pos2(snap(rect.left() + (total.x - w) * 0.5), snap(rect.top() + lay.tops[i]));
                    Rect::from_min_size(min, vec2(w, h))
                };

                cur_page = lay.page_at(viewport.top() + viewport.height() * 0.4).min(n - 1);
                let first = lay.page_at(viewport.top());
                let mut visible = Vec::new();
                for i in first..n {
                    if lay.tops[i] > viewport.bottom() {
                        break;
                    }
                    visible.push(i);
                }

                let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                for &i in &visible {
                    let r = page_rect(i);
                    painter.rect_filled(r.translate(vec2(0.0, 2.0)).expand(1.5), 1.0, Color32::from_black_alpha(45));
                    painter.rect_filled(r, 0.0, Color32::WHITE);
                    let want = want_width(doc.sizes[i], lay.ppt, ppp, max_side);
                    let mut fresh = false;
                    if let Some(t) = doc.tex.get_mut(&i) {
                        t.used = frame_no;
                        fresh = t.width == want;
                        // A bitmap made for this exact size is drawn pixel for pixel.
                        let dst = if fresh { Rect::from_min_size(r.min, t.tex.size_vec2() / ppp) } else { r };
                        painter.image(t.tex.id(), dst, uv, Color32::WHITE);
                    }
                    if !fresh && loaded {
                        jobs.push(Job { page: i, kind: Kind::Render(want) });
                    }

                    // Search hits.
                    if let Some(text) = doc.texts.get(&i) {
                        let start = search.hits.partition_point(|h| h.page < i);
                        for (k, h) in search.hits[start..].iter().enumerate() {
                            if h.page != i {
                                break;
                            }
                            let is_cur = search.cur == Some(start + k);
                            let color = if is_cur {
                                Color32::from_rgba_unmultiplied(255, 120, 0, 110)
                            } else {
                                Color32::from_rgba_unmultiplied(255, 225, 0, 90)
                            };
                            for q in text.rects(h.a, h.b) {
                                let hr = Rect::from_min_max(
                                    r.min + vec2(q[0], q[1]) * lay.ppt,
                                    r.min + vec2(q[2], q[3]) * lay.ppt,
                                );
                                painter.rect_filled(hr, 1.0, color);
                            }
                        }
                        if let Some(s) = sel.as_ref().filter(|s| s.page == i) {
                            let (a, b) = (s.anchor.min(s.head), s.anchor.max(s.head));
                            for q in text.rects(a, text.glyph_end(b)) {
                                let hr = Rect::from_min_max(
                                    r.min + vec2(q[0], q[1]) * lay.ppt,
                                    r.min + vec2(q[2], q[3]) * lay.ppt,
                                );
                                painter.rect_filled(hr, 0.0, Color32::from_rgba_unmultiplied(40, 110, 255, 75));
                            }
                        }
                    } else if loaded {
                        jobs.push(Job { page: i, kind: Kind::Text });
                    }

                    // Text being typed, shown where it will land.
                    if let Some(d) = draft.as_ref().filter(|d| d.page == i) {
                        let c = TEXT_COLORS[d.color].1;
                        let color = Color32::from_rgb((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8);
                        let at = r.min + vec2(d.x, d.y) * lay.ppt;
                        let size = d.size * lay.ppt;
                        if d.text.is_empty() {
                            painter.line_segment([at, at + vec2(0.0, size)], Stroke::new(1.5, color));
                        }
                        for (k, line) in d.text.split('\n').enumerate() {
                            painter.text(
                                at + vec2(0.0, k as f32 * size * 1.2),
                                Align2::LEFT_TOP,
                                line,
                                FontId::proportional(size),
                                color,
                            );
                        }
                    }
                }
                // Neighbours are rendered ahead so scrolling does not show blank pages.
                if loaded {
                    for i in [visible.last().map(|l| l + 1), first.checked_sub(1)].into_iter().flatten() {
                        if i < n {
                            let want = want_width(doc.sizes[i], lay.ppt, ppp, max_side);
                            if doc.tex.get(&i).map(|t| t.width) != Some(want) {
                                jobs.push(Job { page: i, kind: Kind::Render(want) });
                            }
                        }
                    }
                }

                // Mouse: select text, or place new text.
                let locate = |pos: Pos2| visible.iter().copied().find(|&i| page_rect(i).contains(pos));
                let in_page = |i: usize, pos: Pos2| (pos - page_rect(i).min) / lay.ppt;
                match tool {
                    Tool::AddText => {
                        if resp.hovered() {
                            ctx.set_cursor_icon(CursorIcon::Crosshair);
                        }
                        if resp.clicked() {
                            if let Some(pos) = resp.interact_pointer_pos() {
                                if let Some(i) = locate(pos) {
                                    let p = in_page(i, pos);
                                    *draft = Some(TextDraft {
                                        page: i,
                                        x: p.x,
                                        y: p.y,
                                        text: String::new(),
                                        size: 12.0,
                                        color: 0,
                                        focus: true,
                                        screen: pos,
                                    });
                                }
                            }
                        }
                    }
                    Tool::Select => {
                        if let Some(pos) = resp.hover_pos() {
                            if let Some(i) = locate(pos) {
                                let p = in_page(i, pos);
                                let over_text = doc.texts.get(&i).is_some_and(|t| {
                                    t.glyphs.iter().any(|g| p.x >= g.x0 - 2.0 && p.x <= g.x1 + 2.0 && p.y >= g.y0 && p.y <= g.y1)
                                });
                                if over_text {
                                    ctx.set_cursor_icon(CursorIcon::Text);
                                }
                            }
                        }
                        if resp.drag_started() {
                            *sel = None;
                            if let Some(pos) = resp.interact_pointer_pos() {
                                if let Some(i) = locate(pos) {
                                    let p = in_page(i, pos);
                                    if let Some(c) = doc.texts.get(&i).and_then(|t| t.nearest_char(p.x, p.y)) {
                                        *sel = Some(Sel { page: i, anchor: c, head: c });
                                    }
                                }
                            }
                        } else if resp.dragged() {
                            if let (Some(s), Some(pos)) = (sel.as_mut(), resp.interact_pointer_pos()) {
                                let p = in_page(s.page, pos);
                                if let Some(c) = doc.texts.get(&s.page).and_then(|t| t.nearest_char(p.x, p.y)) {
                                    s.head = c;
                                }
                            }
                        } else if resp.double_clicked() {
                            if let Some(pos) = resp.interact_pointer_pos() {
                                if let Some(i) = locate(pos) {
                                    let p = in_page(i, pos);
                                    if let Some(t) = doc.texts.get(&i) {
                                        if let Some(c) = t.nearest_char(p.x, p.y) {
                                            let (a, b) = t.word_range(c);
                                            *sel = Some(Sel { page: i, anchor: a, head: b.saturating_sub(1).max(a) });
                                        }
                                    }
                                }
                            }
                        } else if resp.clicked() {
                            *sel = None;
                        }
                    }
                }
            });
            self.scroll = out.state.clamped_offset();
            self.cur_page = cur_page;
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        if let Some(d) = self.draft.as_mut() {
            let mut open = true;
            let mut done = false;
            egui::Window::new("Add text")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .default_pos(d.screen + vec2(24.0, 60.0))
                .show(ctx, |ui| {
                    let r = ui.add(egui::TextEdit::multiline(&mut d.text).desired_rows(3).desired_width(300.0));
                    if std::mem::take(&mut d.focus) {
                        r.request_focus();
                    }
                    ui.horizontal(|ui| {
                        ui.label("Size");
                        ui.add(egui::DragValue::new(&mut d.size).range(5.0..=96.0).speed(0.25));
                        ui.separator();
                        for (i, (name, _)) in TEXT_COLORS.iter().enumerate() {
                            ui.selectable_value(&mut d.color, i, *name);
                        }
                    });
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!d.text.trim().is_empty(), egui::Button::new("Add to page")).clicked() {
                            done = true;
                        }
                        ui.label("Ctrl+Enter");
                    });
                    if ui.input(|i| i.modifiers.command && i.key_pressed(Key::Enter)) && !d.text.trim().is_empty() {
                        done = true;
                    }
                });
            if done {
                acts.push(Act::AddText);
            } else if !open {
                self.draft = None;
                self.tool = Tool::Select;
            }
        }

        if let Some((pw, wrong)) = self.password.as_mut() {
            let mut answer = None;
            egui::Modal::new(egui::Id::unique("password")).show(ctx, |ui| {
                ui.set_width(300.0);
                ui.heading("Password needed");
                ui.label(if *wrong { "That password did not open the file." } else { "This PDF is protected with a password." });
                ui.add_space(6.0);
                let r = ui.add(egui::TextEdit::singleline(pw).password(true).desired_width(f32::INFINITY));
                r.request_focus();
                let enter = ui.input(|i| i.key_pressed(Key::Enter));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Open").clicked() || enter {
                        answer = Some(Some(pw.clone()));
                    }
                    if ui.button("Cancel").clicked() {
                        answer = Some(None);
                    }
                });
            });
            if let Some(a) = answer {
                acts.push(Act::Password(a));
            }
        }

        if let Some(range) = self.extract.as_mut() {
            let n = self.doc.as_ref().map_or(0, |d| d.sizes.len());
            let mut close = false;
            egui::Modal::new(egui::Id::unique("extract")).show(ctx, |ui| {
                ui.set_width(320.0);
                ui.heading("Save pages as a new PDF");
                ui.label(format!("Pages to save, for example 1-3, 5 (this file has {n})"));
                ui.add_space(6.0);
                ui.add(egui::TextEdit::singleline(range).desired_width(f32::INFINITY)).request_focus();
                let pages = parse_pages(range, n);
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(pages.is_some(), egui::Button::new("Save...")).clicked() {
                        if let Some(p) = pages {
                            acts.push(Act::Extract(p));
                        }
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
            if close {
                self.extract = None;
            }
        }

        if self.confirm_close {
            let name = self.doc.as_ref().map(|d| d.name()).unwrap_or_default();
            egui::Modal::new(egui::Id::unique("close")).show(ctx, |ui| {
                ui.set_width(340.0);
                ui.heading("Save changes?");
                ui.label(format!("{name} has changes that are not saved."));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        acts.push(Act::CloseAnswer(0));
                    }
                    if ui.button("Do not save").clicked() {
                        acts.push(Act::CloseAnswer(1));
                    }
                    if ui.button("Cancel").clicked() {
                        acts.push(Act::CloseAnswer(2));
                    }
                });
            });
        }

        if let Some(msg) = self.error.clone() {
            egui::Modal::new(egui::Id::unique("error")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.heading("ProtoPDF");
                ui.label(msg);
                ui.add_space(8.0);
                if ui.button("OK").clicked() || ui.input(|i| i.key_pressed(Key::Enter) || i.key_pressed(Key::Escape)) {
                    self.error = None;
                }
            });
        }
    }

    fn submit_jobs(&mut self) {
        let Some(doc) = self.doc.as_ref() else { return };
        let mut jobs = std::mem::take(&mut self.jobs_view);
        jobs.append(&mut self.jobs_thumb);
        if doc.loaded && !self.search.active.is_empty() {
            // Search needs the text of every page, nearest pages first.
            let n = doc.sizes.len();
            let cur = self.cur_page;
            let mut rest: Vec<usize> = (0..n).filter(|p| !doc.texts.contains_key(p)).collect();
            rest.sort_by_key(|p| if *p >= cur { p - cur } else { n + p });
            jobs.extend(rest.into_iter().take(64).map(|page| Job { page, kind: Kind::Text }));
        }
        doc.pool.set_jobs(jobs);
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match &self.doc {
            Some(d) => format!("{}{} - ProtoPDF", d.name(), if d.dirty() { " *" } else { "" }),
            None => "ProtoPDF".to_string(),
        };
        if title != self.title {
            self.title = title.clone();
            ctx.send_viewport_cmd(ViewportCommand::Title(title));
        }
    }

    /// `--screenshot file.png`: save a picture of the window once the view has settled, then exit.
    fn screenshot(&mut self, ctx: &egui::Context) {
        if self.shot.is_none() && self.script.is_empty() {
            return;
        }
        ctx.request_repaint();
        let settled = match &self.doc {
            Some(d) => d.loaded && !d.pool.busy() && !self.busy(),
            None => true,
        };
        self.shot_wait = if settled { self.shot_wait + 1 } else { 0 };
        if self.shot_wait == 12 && !self.script.is_empty() {
            let step = self.script.pop().unwrap();
            self.shot_wait = 0;
            self.run_step(ctx, &step);
            return;
        }
        let Some(path) = self.shot.clone() else { return };
        if self.shot_wait == 20 {
            ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                let (w, h) = (img.width() as u32, img.height() as u32);
                let bytes: Vec<u8> = img.pixels.iter().flat_map(|p| p.to_srgba_unmultiplied()).collect();
                let _ = image::save_buffer(&path, &bytes, w, h, image::ColorType::Rgba8);
                std::process::exit(0);
            })));
        }
    }
}

impl App {
    /// One `--do` step, e.g. `rotate`, `text:40,60,Hello`, `saveas:out.pdf`.
    fn run_step(&mut self, ctx: &egui::Context, step: &str) {
        let (name, arg) = step.split_once(':').unwrap_or((step, ""));
        let act = match name {
            "rotate" => Act::Rotate(90, false),
            "rotateall" => Act::Rotate(90, true),
            "delete" => Act::Delete,
            "movedown" => Act::MovePage(true),
            "moveup" => Act::MovePage(false),
            "undo" => Act::Undo,
            "redo" => Act::Redo,
            "selectall" => Act::SelectAll,
            "copy" => Act::Copy,
            "fitpage" => Act::Fit(Fit::Page),
            "fitwidth" => Act::Fit(Fit::Width),
            "zoom" => Act::Zoom(arg.parse().unwrap_or(1.0)),
            "page" => Act::Jump(arg.parse::<usize>().unwrap_or(1).saturating_sub(1)),
            "extract" => match parse_pages(arg, self.doc.as_ref().map_or(0, |d| d.sizes.len())) {
                Some(p) => Act::Extract(p),
                None => return,
            },
            "saveas" => {
                self.save_to(Path::new(arg));
                return;
            }
            "text" => {
                let mut it = arg.splitn(3, ',');
                let x = it.next().and_then(|v| v.parse().ok()).unwrap_or(40.0);
                let y = it.next().and_then(|v| v.parse().ok()).unwrap_or(40.0);
                self.draft = Some(TextDraft {
                    page: self.cur_page,
                    x,
                    y,
                    text: it.next().unwrap_or("").replace("\\n", "\n"),
                    size: 14.0,
                    color: 1,
                    focus: false,
                    screen: pos2(300.0, 300.0),
                });
                Act::AddText
            }
            _ => return,
        };
        self.act(ctx, act);
    }
}

/// The Print toolbar button: a printer drawn with shapes, so it looks the same
/// whatever fonts are installed.
fn printer_button(ui: &mut egui::Ui, enabled: bool) -> egui::Response {
    let h = ui.spacing().interact_size.y;
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(h + 12.0, h), sense);
    if ui.is_rect_visible(rect) {
        let (bg, ink, radius) = if enabled {
            let v = ui.style().interact(&resp);
            (v.weak_bg_fill, v.fg_stroke.color, v.corner_radius)
        } else {
            let v = &ui.visuals().widgets.inactive;
            (v.weak_bg_fill.gamma_multiply(0.5), v.fg_stroke.color.gamma_multiply(0.4), v.corner_radius)
        };
        let p = ui.painter();
        p.rect_filled(rect, radius, bg);
        let c = rect.center();
        let at = |x0: f32, y0: f32, x1: f32, y1: f32| Rect::from_min_max(c + vec2(x0, y0), c + vec2(x1, y1));
        let line = Stroke::new(1.3, ink);
        // Sheet going in at the top, the printer body, and the printed sheet coming out.
        p.rect_stroke(at(-4.0, -7.5, 4.0, -2.5), 0.5, line, StrokeKind::Inside);
        p.rect_filled(at(-8.0, -3.5, 8.0, 3.5), 1.5, ink);
        p.rect_filled(at(-4.5, 0.5, 4.5, 7.5), 0.5, bg);
        p.rect_stroke(at(-4.5, 0.5, 4.5, 7.5), 0.5, line, StrokeKind::Inside);
        p.line_segment([c + vec2(-2.0, 3.5), c + vec2(2.0, 3.5)], Stroke::new(1.0, ink));
        p.line_segment([c + vec2(-2.0, 5.5), c + vec2(2.0, 5.5)], Stroke::new(1.0, ink));
    }
    resp
}

/// Parse "1-3, 5" into 0-based page indices. `None` if anything is out of range.
fn parse_pages(s: &str, n: usize) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    for part in s.split([',', ';', ' ']).filter(|p| !p.is_empty()) {
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (a.trim().parse::<usize>().ok()?, b.trim().parse::<usize>().ok()?),
            None => {
                let v = part.trim().parse::<usize>().ok()?;
                (v, v)
            }
        };
        if a < 1 || b < a || b > n {
            return None;
        }
        out.extend(a - 1..b);
    }
    (!out.is_empty()).then_some(out)
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.frame += 1;
        self.poll(&ctx);

        let mut acts = Vec::new();
        self.shortcuts(&ctx, &mut acts);
        self.top_bar(ui, &mut acts);
        self.status_bar(ui);
        if self.show_thumbs && self.doc.as_ref().is_some_and(|d| !d.sizes.is_empty()) {
            self.thumbs_panel(ui, &mut acts);
        }
        self.pages(ui, &mut acts);
        self.dialogs(&ctx, &mut acts);
        for act in acts {
            self.act(&ctx, act);
        }
        self.submit_jobs();
        self.update_title(&ctx);
        self.screenshot(&ctx);
    }
}
