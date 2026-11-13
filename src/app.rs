//! The window: toolbar, page view, thumbnails, search and dialogs.

use crate::edit::{self, FormValue, Op};
use crate::info::{DocInfo, Target, WidgetKind};
use crate::print;
use crate::register;
use crate::render::{Job, Kind, OpenError, Out, Pool};
use crate::settings::Settings;
use crate::sign::{self, Level, SigInfo};
use crate::text::{PageText, lower_chars};
use crate::theme::{self, Icon, Palette};
use eframe::egui::{
    self, Align, Align2, Color32, CursorIcon, FontId, Frame, Key, Layout as UiLayout, Margin,
    Modifiers, Pos2, Rect, RichText, ScrollArea, Sense, Shadow, Stroke, StrokeKind, TextureHandle,
    TextureOptions, Vec2, ViewportCommand, pos2, vec2,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use lopdf::ObjectId;
use std::sync::mpsc::{Receiver, channel};

/// Screen pixels per PDF point at 100% zoom.
const PT_TO_PX: f32 = 96.0 / 72.0;
const GAP: f32 = 14.0;
const MARGIN: f32 = 18.0;
const THUMB_PX: u32 = 180;
const SIDEBAR_W: f32 = 248.0;
const PANEL_W: f32 = 300.0;
/// Room left under the last page and above a fitted page for the floating find bar.
const BOTTOM_PAD: f32 = 170.0;
const FIND_BAR_ROOM: f32 = 84.0;
const TEX_BUDGET: usize = 420 << 20;
const ZOOMS: [f32; 16] =
    [0.25, 0.33, 0.5, 0.67, 0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0, 6.0];
/// Size of a visible signature, in points.
const SIG_W: f32 = 190.0;
const SIG_H: f32 = 48.0;
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
    PlaceSig,
    AddNote,
}

#[derive(PartialEq, Clone, Copy)]
enum SideTab {
    Pages,
    Contents,
    Comments,
}

/// A sticky note being written.
struct NoteDraft {
    page: usize,
    x: f32,
    y: f32,
    text: String,
    focus: bool,
    screen: Pos2,
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

/// A signature being set up: where it goes and what it says.
struct SignDraft {
    page: usize,
    x: f32,
    y: f32,
    visible: bool,
    reason: String,
    location: String,
    screen: Pos2,
}

enum EditDone {
    Signed(Result<Vec<u8>, String>, PathBuf),
    Replace(Result<Vec<u8>, String>, Option<usize>, &'static str),
    Export(Result<(), String>, PathBuf),
}

enum Act {
    Open,
    OpenPath(PathBuf),
    Save,
    SaveAs,
    Print,
    /// The print dialog was confirmed.
    PrintGo(print::PrintJob),
    /// Windows: choose the certificate to sign with.
    #[cfg(windows)]
    PickCert,
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
    Highlight,
    ToolNote,
    AddNote,
    DeleteAnnot(usize, ObjectId),
    /// Go to a page and a height on it (points from its top), remembering where we were.
    Goto(usize, f32),
    Back,
    OpenUrl(String),
    CommitForm,
    ToolSign,
    SignGo,
    ShowSigs,
    EditSigned(bool),
    ToggleThumbs,
    TogglePanel,
    ToggleTheme,
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
    /// The open print dialog.
    print_setup: Option<print::Setup>,
    /// The frame at which the window's title bar is made to match the theme.
    title_bar_at: u64,
    /// Windows: the certificate to sign with, and its name.
    #[cfg(windows)]
    sign_cert: Option<(sign::Cert, String)>,
    /// What the visible signature will look like, for the box shown while
    /// placing it: what it was made from, and the image (none if it failed).
    sig_preview: Option<(String, Option<TextureHandle>)>,
    /// Linux: the certificate file (.p12 / .pfx) to sign with, and its password.
    #[cfg(not(windows))]
    p12: (Option<PathBuf>, String),
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
    /// Digital signatures found in the open document, already checked.
    sigs: Vec<SigInfo>,
    sig_rx: Option<Receiver<(Arc<Vec<u8>>, Vec<SigInfo>)>>,
    show_sigs: bool,
    sign_draft: Option<SignDraft>,
    /// The user agreed to change a signed document.
    edit_signed_ok: bool,
    pending_edit: Option<(Op, Option<usize>, &'static str)>,
    /// Links, contents, comments and form fields of the open document.
    info: Option<Arc<DocInfo>>,
    info_rx: Option<Receiver<(Arc<Vec<u8>>, Option<DocInfo>)>>,
    side_tab: SideTab,
    /// Form values typed in but not yet written into the document, by field.
    form_edits: HashMap<ObjectId, FormValue>,
    /// The text field being typed in (its widget id).
    active_field: Option<ObjectId>,
    focus_field: bool,
    /// An open drop-down: widget, its place on screen, and the frame it opened in.
    choice_open: Option<(ObjectId, Rect, u64)>,
    /// What to do once the form values have been written into the document.
    after_edit: Option<Act>,
    queued: Vec<Act>,
    note_draft: Option<NoteDraft>,
    /// The comment whose text is shown: page, index on the page, screen position.
    note_open: Option<(usize, usize, Pos2)>,
    /// Places to return to after following links: page and height on it.
    back: Vec<(usize, f32)>,
    jump_offset: f32,
    /// What the pointer is over (a link target), shown at the top of the page view.
    hover: Option<String>,
    settings: Settings,
    pal: Palette,
    /// The document panel on the right.
    show_panel: bool,
    /// The signature notice above the find bar was closed.
    sig_hidden: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, file: Option<PathBuf>, shot: Option<PathBuf>) -> Self {
        // Ctrl +/- zoom the document, not the whole interface.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let args: Vec<String> = std::env::args_os().map(|a| a.to_string_lossy().into_owned()).collect();
        let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
        let mut settings = Settings::load();
        // --theme light|dark: for screenshots; not remembered.
        if let Some(t) = arg("--theme") {
            settings.light = t == "light";
            settings.persist = false;
        }
        theme::install(&cc.egui_ctx, !settings.light);
        let pal = theme::palette(!settings.light);
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
            print_setup: None,
            title_bar_at: 4,
            #[cfg(windows)]
            sign_cert: None,
            sig_preview: None,
            #[cfg(not(windows))]
            p12: (None, String::new()),
            confirm_close: false,
            force_close: false,
            jobs_view: Vec::new(),
            jobs_thumb: Vec::new(),
            frame: 0,
            title: String::new(),
            shot,
            shot_wait: 0,
            script: Vec::new(),
            sigs: Vec::new(),
            sig_rx: None,
            show_sigs: false,
            sign_draft: None,
            edit_signed_ok: false,
            pending_edit: None,
            info: None,
            info_rx: None,
            side_tab: SideTab::Pages,
            form_edits: HashMap::new(),
            active_field: None,
            focus_field: false,
            choice_open: None,
            after_edit: None,
            queued: Vec::new(),
            note_draft: None,
            note_open: None,
            back: Vec::new(),
            jump_offset: 0.0,
            hover: None,
            // Every file opens with the sidebar shown and the document panel hidden.
            show_panel: false,
            settings,
            pal,
            sig_hidden: false,
        };
        app.show_thumbs = true;
        // --panel: start with the document panel open (for screenshots).
        if args.iter().any(|a| a == "--panel") {
            app.show_panel = true;
        }
        // The find box is always on screen.
        app.search.open = true;
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
                self.sign_draft = None;
                self.edit_signed_ok = false;
                self.pending_edit = None;
                self.show_sigs = false;
                self.back.clear();
                self.sig_hidden = false;
                self.check_signatures(ctx);
                self.read_info(ctx);
                self.settings.add_recent(path);
            }
            Err(e) => self.error = Some(format!("Cannot open {}\n{e}", path.display())),
        }
    }

    /// Read links, contents, comments and form fields of the current bytes, off the UI thread.
    fn read_info(&mut self, ctx: &egui::Context) {
        self.info = None;
        self.info_rx = None;
        self.form_edits.clear();
        self.active_field = None;
        self.choice_open = None;
        self.note_open = None;
        self.note_draft = None;
        let Some(doc) = &self.doc else { return };
        let (bytes, password) = (doc.bytes.clone(), doc.password.clone());
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let info = std::panic::catch_unwind(|| crate::info::extract(&bytes, &password)).unwrap_or(None);
            let _ = tx.send((bytes, info));
            ctx.request_repaint();
        });
        self.info_rx = Some(rx);
    }

    fn is_dirty(&self) -> bool {
        self.doc.as_ref().is_some_and(|d| d.dirty()) || !self.form_edits.is_empty()
    }

    /// Write the typed form values into the document, then carry on with `then`.
    fn commit_form(&mut self, ctx: &egui::Context, then: Option<Act>) {
        if self.form_edits.is_empty() || self.busy() || !self.doc.as_ref().is_some_and(|d| d.loaded) {
            return;
        }
        let values = self.form_edits.iter().map(|(k, v)| (*k, v.clone())).collect();
        self.after_edit = then;
        self.active_field = None;
        self.run_edit(ctx, Op::FillForm { values }, None, "Form filled in");
    }

    /// Where the view is now: page and height on it in points.
    fn position(&self) -> Option<(usize, f32)> {
        let doc = self.doc.as_ref()?;
        if doc.sizes.is_empty() {
            return None;
        }
        let lay = layout(&doc.sizes, self.zoom);
        let page = lay.page_at(self.scroll.y);
        Some((page, ((self.scroll.y - lay.tops[page]) / lay.ppt).max(0.0)))
    }

    /// Look for digital signatures in the current bytes and check them, off the UI thread.
    fn check_signatures(&mut self, ctx: &egui::Context) {
        self.sigs.clear();
        self.sig_rx = None;
        let Some(doc) = &self.doc else { return };
        let bytes = doc.bytes.clone();
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let sigs = if sign::has_signature_marker(&bytes) {
                std::panic::catch_unwind(|| sign::verify_all(&bytes)).unwrap_or_default()
            } else {
                Vec::new()
            };
            let _ = tx.send((bytes, sigs));
            ctx.request_repaint();
        });
        self.sig_rx = Some(rx);
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
        self.sign_draft = None;
        self.reset_search();
        self.check_signatures(ctx);
        self.read_info(ctx);
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

        if let Some(rx) = &self.info_rx {
            if let Ok((bytes, info)) = rx.try_recv() {
                self.info_rx = None;
                if self.doc.as_ref().is_some_and(|d| Arc::ptr_eq(&d.bytes, &bytes)) {
                    self.info = info.map(Arc::new);
                }
            }
        }

        if let Some(rx) = &self.sig_rx {
            if let Ok((bytes, sigs)) = rx.try_recv() {
                self.sig_rx = None;
                if self.doc.as_ref().is_some_and(|d| Arc::ptr_eq(&d.bytes, &bytes)) {
                    self.sigs = sigs;
                }
            }
        }

        if let Some(rx) = &self.edit_rx {
            if let Ok(done) = rx.try_recv() {
                self.edit_rx = None;
                match done {
                    EditDone::Signed(Ok(bytes), path) => {
                        // The signed file is on disk already: it becomes the open document.
                        let snap = Arc::new(bytes);
                        if let Some(doc) = self.doc.as_mut() {
                            doc.undo.clear();
                            doc.redo.clear();
                            doc.saved = snap.clone();
                            doc.path = Some(path.clone());
                        }
                        self.replace(ctx, (snap, String::new()), None);
                        self.edit_signed_ok = false;
                        self.status = format!("Signed and saved as {}", path.display());
                    }
                    EditDone::Signed(Err(e), _) => {
                        self.status.clear();
                        self.error = Some(e);
                    }
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
                        if let Some(next) = self.after_edit.take() {
                            self.queued.push(next);
                        }
                    }
                    EditDone::Replace(Err(e), _, _) => {
                        self.status.clear();
                        self.error = Some(e);
                        self.after_edit = None;
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
        if target < self.scroll.y + 20.0 || target > self.scroll.y + self.view_size.y - 80.0 - self.bottom_room() {
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
                let avail_h = (self.view_size.y - 2.0 * GAP - self.bottom_room()).max(50.0);
                (avail_w / (w * PT_TO_PX)).min(avail_h / (h * PT_TO_PX))
            }
        }
        .clamp(0.1, 6.0)
    }

    /// Height covered by the floating find bar, and by the notice above it when one shows.
    fn bottom_room(&self) -> f32 {
        let signed_notice = !self.sigs.is_empty() && !self.sig_hidden && self.sig_rx.is_none();
        FIND_BAR_ROOM + if signed_notice || self.tool != Tool::Select { 54.0 } else { 0.0 }
    }

    fn busy(&self) -> bool {
        self.edit_rx.is_some()
    }

    fn run_edit(&mut self, ctx: &egui::Context, op: Op, jump: Option<usize>, label: &'static str) {
        let Some(doc) = &self.doc else { return };
        if self.busy() || !doc.loaded {
            return;
        }
        if !self.sigs.is_empty() && !self.edit_signed_ok {
            // Any change breaks the signatures, so ask first.
            self.pending_edit = Some((op, jump, label));
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
                self.status = format!("Saved {}", path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().to_string()));
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

    /// Windows: sign with this certificate from now on, also next time.
    #[cfg(windows)]
    fn use_cert(&mut self, cert: sign::Cert) {
        let thumb = cert.thumbprint();
        if !thumb.is_empty() && thumb != self.settings.sign_cert {
            self.settings.sign_cert = thumb;
            self.settings.save();
        }
        let name = cert.name();
        self.sign_cert = Some((cert, name));
    }

    /// The name the new signature will show.
    fn signer_name(&self) -> String {
        #[cfg(windows)]
        if let Some((_, name)) = &self.sign_cert {
            return name.clone();
        }
        "Your name".into()
    }

    /// Draw what the visible signature will look like, with the same code that
    /// signs, at the size the page is shown. Only redone when something changes.
    fn update_sig_preview(&mut self, ctx: &egui::Context, px_per_pt: f32) {
        let Some(d) = self.sign_draft.as_ref().filter(|d| d.visible) else {
            self.sig_preview = None;
            return;
        };
        let name = self.signer_name();
        let px = ((SIG_W * px_per_pt / 128.0).ceil() * 128.0).clamp(256.0, 2048.0) as u32;
        let minute = chrono::Local::now().format("%Y%m%d%H%M");
        let key = format!("{name}\u{1}{}\u{1}{minute}\u{1}{px}", d.reason.trim());
        if self.sig_preview.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        let img = sign::preview_pdf(SIG_W, SIG_H, &name, &d.reason)
            .ok()
            .and_then(|pdf| crate::render::render_one(Arc::new(pdf), "", 0, px, 4096));
        let tex = img.map(|(w, h, mut rgba)| {
            // Rendered on white; take the white out so the page shows through, as it will in the signed file.
            for c in rgba.chunks_exact_mut(4) {
                let a = 255 - c[0].min(c[1]).min(c[2]);
                if a == 0 {
                    c.fill(0);
                } else {
                    for v in &mut c[..3] {
                        *v = ((*v as i32 - (255 - a as i32)).max(0) * 255 / a as i32) as u8;
                    }
                    c[3] = a;
                }
            }
            ctx.load_texture("sig_preview", egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba), TextureOptions::LINEAR)
        });
        self.sig_preview = Some((key, tex));
    }

    fn start_print(&mut self, ctx: &egui::Context, job: print::PrintJob) {
        let Some(doc) = &self.doc else { return };
        let (tx, rx) = channel();
        let ctx2 = ctx.clone();
        print::start(job, doc.bytes.clone(), doc.password.clone(), doc.name(), tx, move || ctx2.request_repaint());
        self.print_rx = Some(rx);
        self.status = "Printing...".into();
    }

    fn act(&mut self, ctx: &egui::Context, act: Act) {
        let n = self.doc.as_ref().map_or(0, |d| d.sizes.len());
        let cur = self.cur_page.min(n.saturating_sub(1));
        // Anything that saves, prints or changes the document first takes in what was typed into the form.
        let uses_document = matches!(
            act,
            Act::Save
                | Act::SaveAs
                | Act::Print
                | Act::Undo
                | Act::Rotate(..)
                | Act::Delete
                | Act::MovePage(_)
                | Act::InsertPdf(_)
                | Act::Extract(_)
                | Act::AddText
                | Act::Highlight
                | Act::AddNote
                | Act::DeleteAnnot(..)
                | Act::SignGo
                | Act::CloseAnswer(0)
        );
        if uses_document && !self.form_edits.is_empty() {
            self.commit_form(ctx, Some(act));
            return;
        }
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
                #[cfg(windows)]
                {
                    self.print_setup = Some(print::Setup::new(doc.sizes.clone(), cur, doc.bytes.clone(), doc.password.clone(), ctx));
                }
                #[cfg(not(windows))]
                {
                    self.print_setup = Some(print::Setup::new(n, cur));
                }
            }
            #[cfg(windows)]
            Act::PickCert => match sign::pick_certificate() {
                Ok(Some(cert)) => self.use_cert(cert),
                Ok(None) => {}
                Err(e) => self.error = Some(e),
            },
            Act::PrintGo(job) => {
                self.print_setup = None;
                if self.doc.as_ref().is_some_and(|d| d.loaded) && self.print_rx.is_none() {
                    self.start_print(ctx, job);
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
            Act::TogglePanel => {
                self.show_panel = !self.show_panel;
            }
            Act::ToggleTheme => {
                self.settings.light = !self.settings.light;
                self.pal = theme::palette(!self.settings.light);
                theme::set_dark(ctx, !self.settings.light);
                self.title_bar_at = self.frame + 3;
                self.settings.save();
            }
            Act::MakeDefault => {
                match std::env::current_exe().map_err(|e| e.to_string()).and_then(|exe| register::register(&exe)).and_then(|()| register::make_default()) {
                    Ok(msg) => self.status = msg,
                    Err(e) => self.error = Some(format!("Could not make ProtoPDF the default PDF app\n{e}")),
                }
            }
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
            Act::CommitForm => self.commit_form(ctx, None),
            Act::Highlight => {
                let Some(sel) = self.sel.as_ref() else {
                    self.status = "Select the text to highlight first".into();
                    return;
                };
                let Some(text) = self.doc.as_ref().and_then(|d| d.texts.get(&sel.page)) else { return };
                let (a, b) = (sel.anchor.min(sel.head), sel.anchor.max(sel.head));
                let rects = text.rects(a, text.glyph_end(b));
                let page = sel.page;
                self.sel = None;
                self.run_edit(ctx, Op::Highlight { page, rects, color: [1.0, 0.92, 0.23] }, None, "Highlighted");
            }
            Act::ToolNote => {
                self.tool = if self.tool == Tool::AddNote { Tool::Select } else { Tool::AddNote };
                self.note_draft = None;
                self.draft = None;
                self.sign_draft = None;
                if self.tool == Tool::AddNote {
                    self.status = "Click on the page where the note should go".into();
                }
            }
            Act::AddNote => {
                if let Some(d) = self.note_draft.take() {
                    self.tool = Tool::Select;
                    if !d.text.trim().is_empty() {
                        self.run_edit(ctx, Op::AddNote { page: d.page, x: d.x, y: d.y, text: d.text }, None, "Note added");
                    }
                }
            }
            Act::DeleteAnnot(page, id) => {
                self.note_open = None;
                self.run_edit(ctx, Op::DeleteAnnot { page, id }, None, "Removed");
            }
            Act::Goto(page, y) => {
                if n > 0 {
                    if let Some(here) = self.position() {
                        self.back.push(here);
                        if self.back.len() > 50 {
                            self.back.remove(0);
                        }
                    }
                    self.jump = Some(page.min(n - 1));
                    self.jump_offset = y;
                }
            }
            Act::Back => {
                if let Some((page, y)) = self.back.pop() {
                    self.jump = Some(page);
                    self.jump_offset = y;
                }
            }
            Act::OpenUrl(url) => {
                if !register::open_url(&url) {
                    self.status = format!("This link was not opened: {url}");
                }
            }
            Act::ToolSign => {
                self.tool = if self.tool == Tool::PlaceSig { Tool::Select } else { Tool::PlaceSig };
                self.sign_draft = None;
                self.draft = None;
                if self.tool == Tool::PlaceSig {
                    self.status = "Click on the page where the signature should appear".into();
                    // The certificate used last time, so the preview shows the right name at once.
                    #[cfg(windows)]
                    if self.sign_cert.is_none() && !self.settings.sign_cert.is_empty() {
                        if let Some(cert) = sign::find_certificate(&self.settings.sign_cert) {
                            let name = cert.name();
                            self.sign_cert = Some((cert, name));
                        }
                    }
                }
            }
            Act::SignGo => {
                if self.sign_draft.is_none() || self.busy() || !self.doc.as_ref().is_some_and(|d| d.loaded) {
                    return;
                }
                #[cfg(windows)]
                let cert = match self.sign_cert.as_ref() {
                    Some((cert, _)) => cert.clone(),
                    None => match sign::pick_certificate() {
                        Ok(Some(cert)) => {
                            self.use_cert(cert.clone());
                            cert
                        }
                        Ok(None) => return,
                        Err(e) => {
                            self.error = Some(e);
                            return;
                        }
                    },
                };
                let (Some(d), Some(doc)) = (self.sign_draft.as_ref(), self.doc.as_ref()) else { return };
                #[cfg(not(windows))]
                let cert = match self.p12.0.as_deref() {
                    None => {
                        self.error = Some("Choose your certificate file (.p12 or .pfx) first.".into());
                        return;
                    }
                    Some(file) => match sign::load_p12(file, &self.p12.1) {
                        Ok(cert) => cert,
                        Err(e) => {
                            self.error = Some(e);
                            return;
                        }
                    },
                };
                let options = sign::SignOptions {
                    page: d.page,
                    rect: d.visible.then(|| [d.x, d.y, d.x + SIG_W, d.y + SIG_H]),
                    reason: d.reason.clone(),
                    location: d.location.clone(),
                };
                let bytes = doc.bytes.clone();
                let Some(path) = self.save_as_dialog("-signed") else { return };
                let (tx, rx) = channel();
                let ctx = ctx.clone();
                let target = path.clone();
                std::thread::spawn(move || {
                    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sign::sign_pdf(&bytes, &cert, &options)))
                        .unwrap_or_else(|_| Err("This PDF could not be signed.".into()))
                        .and_then(|out| {
                            std::fs::write(&target, &out).map_err(|e| format!("Cannot save {}\n{e}", target.display()))?;
                            Ok(out)
                        });
                    let _ = tx.send(EditDone::Signed(res, target));
                    ctx.request_repaint();
                });
                self.edit_rx = Some(rx);
                self.sign_draft = None;
                self.tool = Tool::Select;
                self.status = if cfg!(windows) { "Signing... Windows may ask for the PIN of your signature" } else { "Signing..." }.into();
            }
            Act::ShowSigs => self.show_sigs = !self.show_sigs,
            Act::EditSigned(yes) => {
                if let Some((op, jump, label)) = self.pending_edit.take() {
                    if yes {
                        self.edit_signed_ok = true;
                        self.run_edit(ctx, op, jump, label);
                    } else {
                        self.after_edit = None;
                    }
                }
            }
            Act::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        let typing = ctx.egui_wants_keyboard_input();
        let printing = self.print_setup.is_some();
        let modal = printing || self.password.is_some() || self.error.is_some() || self.confirm_close || self.extract.is_some() || self.pending_edit.is_some();
        let hit = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));
        let ctrl = Modifiers::COMMAND;
        let ctrl_shift = Modifiers::COMMAND | Modifiers::SHIFT;

        if ctx.input(|i| i.viewport().close_requested()) && !self.force_close {
            if self.is_dirty() {
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
            if self.choice_open.is_some() || self.note_open.is_some() || self.active_field.is_some() {
                self.choice_open = None;
                self.note_open = None;
                self.active_field = None;
            } else if self.draft.is_some() || self.sign_draft.is_some() || self.note_draft.is_some() || self.tool != Tool::Select {
                if self.tool != Tool::Select {
                    // The "click on the page..." hint no longer applies.
                    self.status.clear();
                }
                self.draft = None;
                self.sign_draft = None;
                self.note_draft = None;
                self.tool = Tool::Select;
            } else if !self.search.query.is_empty() {
                self.search.query.clear();
            } else {
                self.sel = None;
            }
        }
        let zoom = ctx.input(|i| i.zoom_delta());
        if (zoom - 1.0).abs() > 0.001 && self.doc.is_some() {
            acts.push(Act::Zoom(self.zoom * zoom));
        }

        if hit(Modifiers::ALT, Key::ArrowLeft) {
            acts.push(Act::Back);
        }
        if typing || self.draft.is_some() || self.sign_draft.is_some() || self.note_draft.is_some() {
            return;
        }
        if hit(ctrl, Key::H) {
            acts.push(Act::Highlight);
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

    /// Edits since the last save, counted back through the undo list. `None` when
    /// the saved version is not in the list (after undoing past a save).
    fn unsaved_count(&self) -> Option<usize> {
        let d = self.doc.as_ref()?;
        if !d.dirty() {
            return Some(0);
        }
        d.undo.iter().rev().position(|(b, _)| Arc::ptr_eq(b, &d.saved)).map(|k| k + 1)
    }

    fn recent_files(&self, max: usize) -> Vec<PathBuf> {
        self.settings.recent.iter().filter(|p| p.exists()).take(max).cloned().collect()
    }

    /// The menu behind the file name: every command, including the ones without a button.
    fn title_menu(&self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let has = self.doc.as_ref().is_some_and(|d| d.loaded);
        let n = self.doc.as_ref().map_or(0, |d| d.sizes.len());
        let dirty = self.is_dirty();
        let can_undo = self.doc.as_ref().is_some_and(|d| !d.undo.is_empty());
        let can_redo = self.doc.as_ref().is_some_and(|d| !d.redo.is_empty());
        let edit_ok = has && !self.busy();
        let has_sel = self.sel.is_some();
        let cur = self.cur_page;
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
        ui.set_min_width(250.0);
        if item(ui, true, "Open...", "Ctrl+O") {
            acts.push(Act::Open);
        }
        let recent = self.recent_files(8);
        ui.add_enabled_ui(!recent.is_empty(), |ui| {
            ui.menu_button("Open recent", |ui| {
                for p in &recent {
                    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    if ui.button(name).on_hover_text(p.display().to_string()).clicked() {
                        acts.push(Act::OpenPath(p.clone()));
                        ui.close();
                    }
                }
            });
        });
        if item(ui, has && dirty, "Save", "Ctrl+S") {
            acts.push(Act::Save);
        }
        if item(ui, has, "Save as...", "Ctrl+Shift+S") {
            acts.push(Act::SaveAs);
        }
        if item(ui, has, "Print...", "Ctrl+P") {
            acts.push(Act::Print);
        }
        ui.separator();
        ui.menu_button("Edit", |ui| {
            if item(ui, can_undo && edit_ok, "Undo", "Ctrl+Z") {
                acts.push(Act::Undo);
            }
            if item(ui, can_redo && edit_ok, "Redo", "Ctrl+Y") {
                acts.push(Act::Redo);
            }
            ui.separator();
            if item(ui, has_sel, "Copy", "Ctrl+C") {
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
            if item(ui, edit_ok && has_sel, "Highlight the selected text", "Ctrl+H") {
                acts.push(Act::Highlight);
            }
            if item(ui, edit_ok, "Add a note...", "") {
                acts.push(Act::ToolNote);
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
            if item(ui, edit_ok && cur > 0, "Move page up", "") {
                acts.push(Act::MovePage(false));
            }
            if item(ui, edit_ok && cur + 1 < n, "Move page down", "") {
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
        ui.menu_button("Signatures", |ui| {
            if item(ui, has && !self.busy(), "Sign this document...", "") {
                acts.push(Act::ToolSign);
            }
            let label = match self.sigs.len() {
                0 => "This document is not signed".to_string(),
                1 => "Show the signature".to_string(),
                n => format!("Show the {n} signatures"),
            };
            if item(ui, !self.sigs.is_empty(), &label, "") {
                acts.push(Act::ShowSigs);
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
            if item(ui, true, if self.show_thumbs { "Hide the sidebar" } else { "Show the sidebar" }, "") {
                acts.push(Act::ToggleThumbs);
            }
            if item(ui, true, if self.show_panel { "Hide the document panel" } else { "Show the document panel" }, "") {
                acts.push(Act::TogglePanel);
            }
            if item(ui, true, if self.settings.light { "Dark mode" } else { "Light mode" }, "") {
                acts.push(Act::ToggleTheme);
            }
            ui.separator();
            if item(ui, !self.back.is_empty(), "Back to where the link was", "Alt+Left") {
                acts.push(Act::Back);
            }
        });
        ui.separator();
        if register::is_default() {
            item(ui, false, "ProtoPDF is the default PDF app", "");
        } else if item(ui, true, "Make ProtoPDF the default PDF app...", "") {
            acts.push(Act::MakeDefault);
        }
        if item(ui, true, "Exit", "") {
            acts.push(Act::Quit);
        }
    }

    /// File name with its menu, a short status, and the tools as icons.
    fn top_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>, side_open: bool, panel_open: bool) {
        let p = self.pal;
        let has = self.doc.as_ref().is_some_and(|d| d.loaded);
        let can_undo = self.doc.as_ref().is_some_and(|d| !d.undo.is_empty());
        let can_redo = self.doc.as_ref().is_some_and(|d| !d.redo.is_empty());
        let edit_ok = has && !self.busy();
        let printing = self.print_rx.is_some();
        let busy = self.busy() || printing;
        let mut msg = String::new();
        if self.is_dirty() {
            msg.push_str("Edited");
        }
        // While a tool waits for a click its hint is in the notice above the find bar.
        if !self.status.is_empty() && self.tool == Tool::Select {
            if !msg.is_empty() {
                msg.push_str("  ·  ");
            }
            msg.push_str(&self.status);
        }
        let title = self.doc.as_ref().map(|d| d.name()).unwrap_or_else(|| "ProtoPDF".into());
        let frame = Frame::NONE.fill(p.bg).inner_margin(Margin { left: 12, right: 10, top: 0, bottom: 0 });
        egui::Panel::top("top").exact_size(48.0).frame(frame).show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if !side_open {
                    if theme::icon_button(ui, &p, Icon::Sidebar, 32.0, true, false, "Show the sidebar").clicked() {
                        acts.push(Act::ToggleThumbs);
                    }
                    ui.add_space(6.0);
                }
                let galley = ui.painter().layout_job(egui::text::LayoutJob {
                    wrap: egui::text::TextWrapping::truncate_at_width(380.0),
                    ..egui::text::LayoutJob::simple_singleline(title, theme::medium(15.0), p.text)
                });
                let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 36.0, 32.0), Sense::click());
                let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
                if resp.hovered() || open {
                    ui.painter().rect_filled(rect, 8.0, p.hover);
                }
                ui.painter().galley(pos2(rect.left() + 8.0, rect.center().y - galley.size().y * 0.5), galley, p.text);
                theme::paint_icon(ui.painter(), Rect::from_center_size(pos2(rect.right() - 15.0, rect.center().y), vec2(16.0, 16.0)), Icon::ChevronDown, p.muted);
                let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
                egui::Popup::menu(&resp).show(|ui| self.title_menu(ui, acts));
                if busy {
                    ui.add_space(8.0);
                    ui.spinner();
                }

                ui.with_layout(UiLayout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if has {
                        let tip = if panel_open { "Hide the document panel" } else { "Show the document panel" };
                        if theme::icon_button(ui, &p, Icon::PanelRight, 32.0, true, panel_open, tip).clicked() {
                            acts.push(Act::TogglePanel);
                        }
                        theme::vdivider(ui, &p);
                    }
                    if theme::icon_button(ui, &p, Icon::Printer, 32.0, has && !printing, false, "Print (Ctrl+P)").clicked() {
                        acts.push(Act::Print);
                    }
                    if has {
                        theme::vdivider(ui, &p);
                        if theme::icon_button(ui, &p, Icon::Redo, 32.0, can_redo && edit_ok, false, "Redo (Ctrl+Y)").clicked() {
                            acts.push(Act::Redo);
                        }
                        if theme::icon_button(ui, &p, Icon::Undo, 32.0, can_undo && edit_ok, false, "Undo (Ctrl+Z)").clicked() {
                            acts.push(Act::Undo);
                        }
                        if theme::icon_button(ui, &p, Icon::Rotate, 32.0, edit_ok, false, "Rotate this page right (Ctrl+R)").clicked() {
                            acts.push(Act::Rotate(90, false));
                        }
                        theme::vdivider(ui, &p);
                        let signing = self.tool == Tool::PlaceSig;
                        if theme::icon_button(ui, &p, Icon::Sign, 32.0, has && !self.busy(), signing, "Sign with your digital signature").clicked() {
                            acts.push(Act::ToolSign);
                        }
                        if theme::icon_button(ui, &p, Icon::Note, 32.0, edit_ok, self.tool == Tool::AddNote, "Stick a note on the page").clicked() {
                            acts.push(Act::ToolNote);
                        }
                        if theme::icon_button(ui, &p, Icon::Text, 32.0, edit_ok, self.tool == Tool::AddText, "Write text on the page").clicked() {
                            acts.push(Act::ToolAddText);
                        }
                        let tip = if self.sel.is_some() {
                            "Highlight the selected text (Ctrl+H)"
                        } else {
                            "Highlight: select text with the mouse first"
                        };
                        if theme::icon_button(ui, &p, Icon::Highlighter, 32.0, edit_ok && self.sel.is_some(), false, tip).clicked() {
                            acts.push(Act::Highlight);
                        }
                    }
                    ui.with_layout(UiLayout::left_to_right(Align::Center), |ui| {
                        if !msg.is_empty() {
                            ui.add_space(8.0);
                            ui.add(egui::Label::new(RichText::new(msg.as_str()).size(13.0).color(p.muted)).truncate());
                        }
                    });
                });
            });
        });
    }

    /// Left: open / find / print, the page list (or contents, or comments), recent files.
    fn side_panel(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let p = self.pal;
        let has = self.doc.as_ref().is_some_and(|d| d.loaded && !d.sizes.is_empty());
        let cur_path = self.doc.as_ref().and_then(|d| d.path.as_ref()).and_then(|p| std::path::absolute(p).ok());
        let recent = self.recent_files(if has { 4 } else { 8 });
        let can_back = !self.back.is_empty();
        let frame = Frame::NONE.fill(p.side).inner_margin(Margin { left: 8, right: 8, top: 6, bottom: 6 });
        egui::Panel::left("side").resizable(false).exact_size(SIDEBAR_W).frame(frame).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.horizontal(|ui| {
                ui.set_height(40.0);
                ui.spacing_mut().item_spacing.x = 2.0;
                if theme::icon_button(ui, &p, Icon::Sidebar, 32.0, true, false, "Hide the sidebar").clicked() {
                    acts.push(Act::ToggleThumbs);
                }
                if theme::icon_button(ui, &p, Icon::ChevronLeft, 32.0, can_back, false, "Back to where the link was (Alt+Left)").clicked() {
                    acts.push(Act::Back);
                }
            });
            ui.add_space(6.0);
            if theme::nav_row(ui, &p, Icon::Save, "Save as", "", has && !self.busy()).on_hover_text("Save this PDF in a folder you choose").clicked() {
                acts.push(Act::SaveAs);
            }
            if theme::nav_row(ui, &p, Icon::Folder, "Open file", "Ctrl O", true).clicked() {
                acts.push(Act::Open);
            }
            if theme::nav_row(ui, &p, Icon::Search, "Find in document", "Ctrl F", has).clicked() {
                acts.push(Act::Find);
            }

            let footer_h = 46.0;
            let recent_h = if recent.is_empty() { 0.0 } else { 40.0 + recent.len() as f32 * 36.0 };
            if has {
                ui.add_space(16.0);
                let tab = self.side_tabs(ui);
                ui.add_space(10.0);
                let h = (ui.available_height() - recent_h - footer_h - 6.0).max(90.0);
                let w = ui.available_width();
                ui.allocate_ui(vec2(w, h), |ui| {
                    ui.set_min_size(vec2(w, h));
                    match tab {
                        SideTab::Pages => self.thumb_grid(ui, acts),
                        SideTab::Contents => self.outline_list(ui, acts),
                        SideTab::Comments => {
                            ScrollArea::vertical().auto_shrink(false).id_salt("comments").show(ui, |ui| {
                                ui.add_space(2.0);
                                self.comment_list(ui, acts, usize::MAX);
                            });
                        }
                    }
                });
            }
            if !recent.is_empty() {
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.label(RichText::new("Recent").size(13.0).color(p.muted));
                });
                ui.add_space(4.0);
                for r in &recent {
                    let name = r.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let here = cur_path.as_ref() == Some(r);
                    let resp = theme::list_row(ui, &p, &name, here).on_hover_text(r.display().to_string());
                    if resp.clicked() && !here {
                        acts.push(Act::OpenPath(r.clone()));
                    }
                }
            }

            let rest = ui.available_height() - footer_h;
            if rest > 0.0 {
                ui.add_space(rest);
            }
            let (line, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
            ui.painter().rect_filled(line, 0.0, p.border);
            ui.horizontal(|ui| {
                ui.set_height(44.0);
                ui.add_space(8.0);
                let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
                ui.painter().rect_filled(r, 6.0, p.accent);
                theme::paint_icon(ui.painter(), r.shrink(4.0), Icon::File, Color32::WHITE);
                ui.add_space(4.0);
                ui.label(RichText::new("ProtoPDF").font(theme::medium(14.0)).color(p.text));
                ui.label(RichText::new(env!("CARGO_PKG_VERSION")).size(13.0).color(p.muted));
                ui.with_layout(UiLayout::right_to_left(Align::Center), |ui| {
                    let (ic, tip) = if p.dark { (Icon::Sun, "Switch to light mode") } else { (Icon::Moon, "Switch to dark mode") };
                    if theme::icon_button(ui, &p, ic, 32.0, true, false, tip).clicked() {
                        acts.push(Act::ToggleTheme);
                    }
                });
            });
        });
    }

    /// Pages / Contents / Comments. Returns the tab to show (one that has something in it).
    fn side_tabs(&mut self, ui: &mut egui::Ui) -> SideTab {
        let p = self.pal;
        let info = self.info.clone();
        let has_outline = info.as_ref().is_some_and(|i| !i.outline.is_empty());
        let comments: usize = info.as_ref().map_or(0, |i| i.pages.iter().map(|p| p.notes.len()).sum());
        let mut tabs = vec![(SideTab::Pages, "Pages".to_string())];
        if has_outline {
            tabs.push((SideTab::Contents, "Contents".to_string()));
        }
        if comments > 0 {
            tabs.push((SideTab::Comments, format!("Comments {comments}")));
        }
        let shown = if tabs.iter().any(|t| t.0 == self.side_tab) { self.side_tab } else { SideTab::Pages };
        let top = ui.cursor().top();
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            ui.spacing_mut().item_spacing.x = 18.0;
            for (tab, label) in tabs {
                let on = tab == shown;
                let (font, color) = if on { (theme::medium(13.0), p.text) } else { (FontId::proportional(13.0), p.muted) };
                let galley = ui.painter().layout_no_wrap(label, font, color);
                let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x, 32.0), Sense::click());
                ui.painter().galley(pos2(rect.left(), rect.center().y - galley.size().y * 0.5), galley, color);
                if on {
                    ui.painter().rect_filled(Rect::from_min_max(pos2(rect.left(), rect.bottom() - 2.0), rect.right_bottom()), 1.0, p.accent);
                }
                if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    self.side_tab = tab;
                }
            }
        });
        let r = ui.min_rect();
        let y = (top + 32.0).max(r.top());
        ui.painter().rect_filled(Rect::from_min_max(pos2(r.left(), y), pos2(r.right(), y + 1.0)), 0.0, p.border);
        shown
    }

    /// Page thumbnails, two to a row.
    fn thumb_grid(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let p = self.pal;
        let Some(doc) = self.doc.as_mut() else { return };
        let cur = self.cur_page;
        let jobs = &mut self.jobs_thumb;
        let sizes = &doc.sizes;
        let thumbs = &doc.thumbs;
        let loaded = doc.loaded;
        let width = ui.available_width() - 6.0;
        let gap = 10.0;
        let cell = ((width - gap) * 0.5).floor();
        let tw = (cell - 16.0).clamp(40.0, 100.0);
        let th = |s: &(f32, f32)| (tw * (s.1 / s.0.max(1.0)).clamp(0.2, 3.0)).round();
        let rows = sizes.len().div_ceil(2);
        let row_h = |r: usize| {
            let b = sizes.get(2 * r + 1).map(th).unwrap_or(0.0);
            th(&sizes[2 * r]).max(b) + 36.0
        };

        // Keep the current page in sight when it changes.
        if self.thumb_follow != cur && loaded && rows > 0 {
            self.thumb_follow = cur;
            let r = (cur / 2).min(rows - 1);
            let top: f32 = (0..r).map(row_h).sum();
            let (v_top, v_h) = self.thumb_view;
            if top < v_top || top + row_h(r) > v_top + v_h {
                self.thumb_scroll = Some((top - 30.0).max(0.0));
            }
        }

        let mut area = ScrollArea::vertical().auto_shrink(false).id_salt("thumbs");
        if let Some(y) = self.thumb_scroll.take() {
            area = area.vertical_scroll_offset(y);
        }
        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        let out = area.show_viewport(ui, |ui, viewport| {
            let total: f32 = (0..rows).map(row_h).sum();
            let (rect, _) = ui.allocate_exact_size(vec2(width, total.max(viewport.height())), Sense::hover());
            let painter = ui.painter();
            let mut y = 0.0;
            for r in 0..rows {
                let h = row_h(r);
                if y + h >= viewport.top() && y <= viewport.bottom() {
                    for c in 0..2 {
                        let i = 2 * r + c;
                        let Some(s) = sizes.get(i) else { break };
                        let x0 = rect.left() + c as f32 * (cell + gap);
                        let tr = Rect::from_min_size(pos2((x0 + (cell - tw) * 0.5).round(), rect.top() + y + 6.0), vec2(tw, th(s)));
                        let resp = ui.interact(tr.expand(5.0), ui.scope_id().with(("thumb", i)), Sense::click());
                        if resp.clicked() {
                            acts.push(Act::Jump(i));
                        }
                        if i == cur {
                            painter.rect_stroke(tr.expand(5.0), 7.0, Stroke::new(2.0, p.accent), StrokeKind::Inside);
                        } else if resp.hovered() {
                            painter.rect_stroke(tr.expand(5.0), 7.0, Stroke::new(1.0, p.faint), StrokeKind::Inside);
                        }
                        painter.rect_filled(tr, 2.0, Color32::WHITE);
                        match thumbs.get(&i) {
                            Some(tex) => {
                                painter.image(tex.id(), tr, uv, Color32::WHITE);
                            }
                            None if loaded => jobs.push(Job { page: i, kind: Kind::Render(THUMB_PX) }),
                            None => {}
                        }
                        painter.rect_stroke(tr, 2.0, Stroke::new(1.0, Color32::from_black_alpha(40)), StrokeKind::Outside);
                        let (font, color) = if i == cur { (theme::medium(12.0), p.text) } else { (FontId::proportional(12.0), p.muted) };
                        painter.text(pos2(tr.center().x, tr.bottom() + 9.0), Align2::CENTER_TOP, (i + 1).to_string(), font, color);
                    }
                }
                y += h;
            }
        });
        self.thumb_view = (out.state.clamped_offset().y, out.inner_rect.height());
    }

    /// The document's bookmarks, with the one being read marked.
    fn outline_list(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let Some(info) = self.info.clone() else { return };
        let here = self.position().unwrap_or((self.cur_page, 0.0));
        // The entry being read: the last one that starts at or above the top of the view.
        let current = info.outline.iter().rposition(|o| o.page.is_some_and(|p| p < here.0 || (p == here.0 && o.y <= here.1 + 60.0)));
        ScrollArea::vertical().auto_shrink(false).id_salt("outline").show(ui, |ui| {
            for (k, item) in info.outline.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.add_space(4.0 + item.level.min(8) as f32 * 12.0);
                    let entry = egui::Button::selectable(current == Some(k), item.title.as_str()).truncate();
                    if ui.add_enabled(item.page.is_some(), entry).clicked() {
                        if let Some(page) = item.page {
                            acts.push(Act::Goto(page, item.y));
                        }
                    }
                });
            }
        });
    }

    /// Highlights and notes, in page order. Clicking one goes to it.
    fn comment_list(&self, ui: &mut egui::Ui, acts: &mut Vec<Act>, limit: usize) {
        let p = self.pal;
        let Some(info) = self.info.clone() else { return };
        let mut shown = 0;
        for (page, pi) in info.pages.iter().enumerate() {
            for (k, note) in pi.notes.iter().enumerate() {
                if shown == limit {
                    return;
                }
                shown += 1;
                let kind = if note.highlight { "Highlight" } else { "Note" };
                let mut head = format!("{kind} · page {}", page + 1);
                if !note.author.trim().is_empty() {
                    head.push_str(" · ");
                    head.push_str(note.author.trim());
                }
                let body: String = note.text.trim().chars().take(160).collect();
                let resp = ui
                    .horizontal_top(|ui| {
                        ui.add_space(2.0);
                        let (sw, _) = ui.allocate_exact_size(vec2(10.0, 18.0), Sense::hover());
                        let sq = Rect::from_center_size(pos2(sw.center().x, sw.top() + 9.0), vec2(10.0, 10.0));
                        if note.highlight {
                            ui.painter().rect_filled(sq, 3.0, Color32::from_rgb(250, 204, 21));
                        } else {
                            ui.painter().rect(sq, 3.0, Color32::from_rgb(242, 201, 76), Stroke::new(1.0, Color32::from_rgb(184, 144, 26)), StrokeKind::Inside);
                        }
                        ui.add_space(4.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.label(RichText::new(head).size(12.0).color(p.muted));
                            if body.is_empty() {
                                ui.label(RichText::new(if note.highlight { "Highlighted text" } else { "Empty note" }).size(13.5).color(p.muted).italics());
                            } else {
                                ui.add(egui::Label::new(RichText::new(body).size(13.5).color(p.text2)).wrap());
                            }
                        });
                    })
                    .response;
                let click = ui.interact(resp.rect, ui.scope_id().with(("comment", page, k)), Sense::click());
                if click.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    acts.push(Act::Goto(page, (note.rect[1] - 40.0).max(0.0)));
                }
                ui.add_space(8.0);
            }
        }
    }

    /// Right: what the document is, who signed it, its comments and unsaved changes.
    fn info_panel(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let p = self.pal;
        let frame = Frame::NONE.fill(p.bg).inner_margin(Margin { left: 20, right: 14, top: 18, bottom: 14 });
        egui::Panel::right("info").resizable(false).exact_size(PANEL_W).frame(frame).show(ui, |ui| {
            ScrollArea::vertical().auto_shrink(false).id_salt("info").show(ui, |ui| {
                ui.set_width(ui.available_width() - 6.0);
                ui.spacing_mut().item_spacing.y = 4.0;
                self.info_document(ui);
                theme::divider(ui, &p);
                self.info_signatures(ui, acts);
                theme::divider(ui, &p);
                self.info_comments(ui, acts);
                theme::divider(ui, &p);
                self.info_changes(ui, acts);
            });
        });
    }

    fn info_document(&self, ui: &mut egui::Ui) {
        let p = self.pal;
        let Some(doc) = &self.doc else { return };
        theme::section(ui, &p, "Document", None);
        ui.add_space(2.0);
        let row = |ui: &mut egui::Ui, ic: Icon, text: String| {
            ui.horizontal(|ui| {
                ui.set_height(30.0);
                theme::icon(ui, ic, 18.0, p.muted);
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new(text).size(14.0).color(p.text2)).truncate());
            });
        };
        let n = doc.sizes.len();
        let (w, h) = doc.sizes.get(self.cur_page).copied().unwrap_or((0.0, 0.0));
        let pages = format!("{n} {}", if n == 1 { "page" } else { "pages" });
        row(ui, Icon::File, match paper_name(w, h) {
            Some(name) => format!("{pages} · {name}"),
            None => pages,
        });
        row(ui, Icon::Ruler, format!("{:.0} × {:.0} mm", w * 25.4 / 72.0, h * 25.4 / 72.0));
        let lock = if doc.password.is_empty() { "No password" } else { "Password protected" };
        row(ui, Icon::Lock, format!("{lock} · {}", human_size(doc.bytes.len())));
    }

    fn info_signatures(&self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let p = self.pal;
        let has = self.doc.as_ref().is_some_and(|d| d.loaded);
        let sigs = &self.sigs;
        let note = if self.sig_rx.is_some() {
            Some((Icon::Shield, "Checking...".to_string(), p.muted))
        } else if sigs.is_empty() {
            None
        } else {
            Some(match sign::summary(sigs).0 {
                Level::Good => (Icon::Check, format!("{} valid", sigs.len()), p.good),
                Level::Warn => (Icon::Warn, "Not confirmed".to_string(), p.warn),
                Level::Bad => (Icon::Warn, "Not valid".to_string(), p.bad),
            })
        };
        theme::section(ui, &p, "Signatures", note.as_ref().map(|(i, t, c)| (*i, t.as_str(), *c)));
        ui.add_space(6.0);
        if sigs.is_empty() && self.sig_rx.is_none() {
            ui.label(RichText::new("This document is not signed.").size(13.0).color(p.muted));
            ui.add_space(4.0);
        }
        for (k, s) in sigs.iter().enumerate().take(3) {
            let title = if s.is_timestamp {
                "Document timestamp".to_string()
            } else if s.org.is_empty() || s.org == s.signer {
                s.signer.clone()
            } else {
                format!("{} ({})", s.signer, s.org)
            };
            let when = match &s.timestamp {
                Some(t) => format!("{t} · timestamped"),
                None => s.signed_at.clone(),
            };
            let card = Frame::new()
                .fill(p.card)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(12)
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_top(|ui| {
                        theme::avatar(ui, &p, if s.is_timestamp { "T S" } else { s.signer.as_str() });
                        ui.add_space(4.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.add(egui::Label::new(RichText::new(title).font(theme::medium(14.0)).color(p.text)).truncate());
                            if !when.is_empty() {
                                ui.add(egui::Label::new(RichText::new(when).size(12.0).color(p.muted)).truncate());
                            }
                            ui.add_space(6.0);
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                                if s.intact {
                                    theme::chip(ui, Icon::Check, "Unchanged", p.good, p.good_bg);
                                } else {
                                    theme::chip(ui, Icon::Warn, "Changed", p.bad, p.bad_bg);
                                }
                                if s.trusted {
                                    theme::chip(ui, Icon::Check, "Trusted", p.good, p.good_bg);
                                } else {
                                    theme::chip(ui, Icon::Warn, "Not confirmed", p.warn, p.warn_bg);
                                }
                            });
                        });
                    });
                })
                .response;
            let click = ui.interact(card.rect, ui.scope_id().with(("sig", k)), Sense::click());
            if click.on_hover_cursor(CursorIcon::PointingHand).on_hover_text("Show the details").clicked() {
                acts.push(Act::ShowSigs);
            }
            ui.add_space(6.0);
        }
        if sigs.len() > 3 {
            if ui.link(format!("{} more", sigs.len() - 3)).clicked() {
                acts.push(Act::ShowSigs);
            }
        }
        let placing = self.tool == Tool::PlaceSig;
        let label = if placing { "Click the page to place it" } else { "Add my signature" };
        if theme::wide_button(ui, &p, Icon::Sign, p.accent, label, has && !self.busy(), placing).clicked() {
            acts.push(Act::ToolSign);
        }
    }

    fn info_comments(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let p = self.pal;
        let count: usize = self.info.as_ref().map_or(0, |i| i.pages.iter().map(|p| p.notes.len()).sum());
        let n = count.to_string();
        theme::section(ui, &p, "Comments", (count > 0).then_some((Icon::Note, n.as_str(), p.muted)));
        ui.add_space(6.0);
        if count == 0 {
            ui.add(egui::Label::new(RichText::new("None yet. Select text and highlight it, or stick a note on the page.").size(13.0).color(p.muted)).wrap());
            return;
        }
        self.comment_list(ui, acts, 4);
        if count > 4 && ui.link(format!("Show all {count}")).clicked() {
            self.show_thumbs = true;
            self.side_tab = SideTab::Comments;
        }
    }

    fn info_changes(&self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let p = self.pal;
        let dirty = self.is_dirty();
        theme::section(ui, &p, "Changes", None);
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.set_height(34.0);
            theme::icon(ui, Icon::Diff, 18.0, p.muted);
            ui.add_space(4.0);
            if dirty {
                match self.unsaved_count() {
                    Some(k) if k > 0 => {
                        ui.label(RichText::new(format!("+{k}")).font(theme::mono(13.0)).color(p.good));
                        ui.label(RichText::new("not saved").size(13.0).color(p.muted));
                    }
                    _ => {
                        ui.label(RichText::new("Not saved").size(13.0).color(p.muted));
                    }
                }
            } else {
                ui.label(RichText::new("All saved").size(13.0).color(p.muted));
            }
            ui.with_layout(UiLayout::right_to_left(Align::Center), |ui| {
                if theme::primary_button(ui, &p, "Save changes", dirty && !self.busy()).clicked() {
                    acts.push(Act::Save);
                }
            });
        });
    }

    /// Floating over the bottom of the page view: a notice (signature check or what
    /// the current tool wants) and the find bar with page and zoom controls.
    fn overlay(&mut self, ctx: &egui::Context, area: Rect, acts: &mut Vec<Act>) {
        let p = self.pal;
        if !self.doc.as_ref().is_some_and(|d| d.loaded) {
            return;
        }
        if let Some(h) = self.hover.clone() {
            egui::Area::new(egui::Id::unique("hover_link"))
                .interactable(false)
                .fixed_pos(area.left_top() + vec2(12.0, 12.0))
                .show(ctx, |ui| {
                    Frame::new().fill(p.raised).stroke(Stroke::new(1.0, p.border)).corner_radius(8).inner_margin(Margin::symmetric(10, 5)).show(ui, |ui| {
                        ui.label(RichText::new(h).size(12.5).color(p.text2));
                    });
                });
        }
        let width = (area.width() - 32.0).clamp(280.0, 700.0);
        egui::Area::new(egui::Id::unique("find_bar"))
            .pivot(Align2::CENTER_BOTTOM)
            .fixed_pos(pos2(area.center().x, area.bottom() - 16.0))
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing.y = 8.0;
                self.notice(ui, acts, width);
                self.find_bar(ui, acts, width);
            });
    }

    fn notice(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>, width: f32) {
        let p = self.pal;
        let tool_hint = match self.tool {
            Tool::AddText if self.draft.is_none() => Some("Click on the page where the text should start"),
            Tool::AddNote if self.note_draft.is_none() => Some("Click on the page where the note should go"),
            Tool::PlaceSig if self.sign_draft.is_none() => Some("Click on the page where the signature should appear"),
            _ => None,
        };
        let mut job = egui::text::LayoutJob::default();
        let fmt = |color: Color32| egui::TextFormat { font_id: FontId::proportional(14.0), color, ..Default::default() };
        let (ic, color, button, closable) = if let Some(hint) = tool_hint {
            job.append(hint, 0.0, fmt(p.text));
            let ic = match self.tool {
                Tool::AddText => Icon::Text,
                Tool::AddNote => Icon::Note,
                _ => Icon::Sign,
            };
            (ic, p.accent, "Cancel", false)
        } else if !self.sigs.is_empty() && !self.sig_hidden && self.sig_rx.is_none() {
            let (level, text) = sign::summary(&self.sigs);
            let (head, rest) = match text.find(". ") {
                Some(k) => (&text[..k + 1], &text[k + 1..]),
                None => (text.as_str(), ""),
            };
            job.append(head, 0.0, fmt(p.text));
            job.append(rest, 0.0, fmt(p.muted));
            let (ic, color) = match level {
                Level::Good => (Icon::Shield, p.good),
                Level::Warn => (Icon::Warn, p.warn),
                Level::Bad => (Icon::Warn, p.bad),
            };
            (ic, color, "Details", true)
        } else {
            return;
        };
        job.wrap = egui::text::TextWrapping::truncate_at_width(f32::INFINITY);
        Frame::new()
            .fill(if p.dark { p.side } else { p.raised })
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(14)
            .inner_margin(Margin { left: 14, right: 8, top: 7, bottom: 7 })
            .show(ui, |ui| {
                ui.set_width(width - 24.0);
                ui.horizontal(|ui| {
                    theme::icon(ui, ic, 18.0, color);
                    ui.add_space(4.0);
                    ui.with_layout(UiLayout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        if closable && theme::icon_button(ui, &p, Icon::Close, 30.0, true, false, "Hide").clicked() {
                            self.sig_hidden = true;
                        }
                        let b = egui::Button::new(RichText::new(button).size(13.0).color(p.text)).fill(p.active).stroke(Stroke::NONE).corner_radius(8).min_size(vec2(0.0, 30.0));
                        if ui.add(b).clicked() {
                            if closable {
                                acts.push(Act::ShowSigs);
                            } else {
                                self.tool = Tool::Select;
                                self.status.clear();
                            }
                        }
                        ui.with_layout(UiLayout::left_to_right(Align::Center), |ui| {
                            ui.add(egui::Label::new(job).truncate());
                        });
                    });
                });
            });
    }

    fn find_bar(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>, width: f32) {
        let p = self.pal;
        let n = self.doc.as_ref().map_or(0, |d| d.sizes.len());
        let find_id = egui::Id::unique("find_text");
        let focused = ui.memory(|m| m.has_focus(find_id));
        Frame::new()
            .fill(p.raised)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(16)
            .inner_margin(Margin { left: 14, right: 8, top: 7, bottom: 7 })
            .shadow(Shadow { offset: [0, 10], blur: 28, spread: 0, color: p.shadow })
            .show(ui, |ui| {
                ui.set_width(width - 24.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    theme::icon(ui, Icon::Search, 18.0, p.muted);
                    ui.add_space(6.0);
                    ui.with_layout(UiLayout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        if theme::icon_button(ui, &p, Icon::FitPage, 32.0, true, self.fit == Fit::Page, "Fit page (Ctrl+0)").clicked() {
                            acts.push(Act::Fit(Fit::Page));
                        }
                        if theme::icon_button(ui, &p, Icon::FitWidth, 32.0, true, self.fit == Fit::Width, "Fit width (Ctrl+2)").clicked() {
                            acts.push(Act::Fit(Fit::Width));
                        }
                        theme::vdivider(ui, &p);
                        if theme::icon_button(ui, &p, Icon::Plus, 30.0, true, false, "Zoom in (Ctrl++)").clicked() {
                            acts.push(Act::ZoomStep(true));
                        }
                        ui.menu_button(RichText::new(format!("{:.0}%", self.zoom * 100.0)).font(theme::mono(13.0)).color(p.text), |ui| {
                            for z in ZOOMS {
                                if ui.button(format!("{:.0}%", z * 100.0)).clicked() {
                                    acts.push(Act::Zoom(z));
                                    ui.close();
                                }
                            }
                        });
                        if theme::icon_button(ui, &p, Icon::Minus, 30.0, true, false, "Zoom out (Ctrl+-)").clicked() {
                            acts.push(Act::ZoomStep(false));
                        }
                        theme::vdivider(ui, &p);
                        if theme::icon_button(ui, &p, Icon::ChevronRight, 30.0, self.cur_page + 1 < n, false, "Next page").clicked() {
                            acts.push(Act::Jump(self.cur_page + 1));
                        }
                        ui.label(RichText::new(format!("/ {n}")).font(theme::mono(13.0)).color(p.muted));
                        let field = ui.add(
                            egui::TextEdit::singleline(&mut self.page_field)
                                .desired_width(30.0)
                                .align(Align2::CENTER_CENTER)
                                .font(theme::mono(13.0))
                                .margin(Margin::symmetric(4, 3)),
                        );
                        if field.lost_focus() {
                            if let Ok(pg) = self.page_field.trim().parse::<usize>() {
                                if pg >= 1 && pg - 1 != self.cur_page {
                                    acts.push(Act::Jump(pg - 1));
                                }
                            }
                        }
                        if !field.has_focus() {
                            self.page_field = if n > 0 { (self.cur_page + 1).to_string() } else { String::new() };
                        }
                        if theme::icon_button(ui, &p, Icon::ChevronLeft, 30.0, self.cur_page > 0, false, "Previous page").clicked() {
                            acts.push(Act::Jump(self.cur_page.saturating_sub(1)));
                        }
                        theme::vdivider(ui, &p);

                        let s = &self.search;
                        if !s.query.trim().is_empty() {
                            let any = !s.hits.is_empty();
                            if theme::icon_button(ui, &p, Icon::ChevronDown, 28.0, any, false, "Next match (Enter, F3)").clicked() {
                                acts.push(Act::FindNext(true));
                            }
                            if theme::icon_button(ui, &p, Icon::ChevronUp, 28.0, any, false, "Previous match (Shift+Enter)").clicked() {
                                acts.push(Act::FindNext(false));
                            }
                            if !s.active.is_empty() {
                                let pending = n.saturating_sub(s.scanned.len());
                                let msg = match (s.hits.len(), s.cur, pending) {
                                    (0, _, 0) => "Not found".to_string(),
                                    (0, _, _) => "Searching...".to_string(),
                                    (len, Some(i), _) => format!("{} of {len}", i + 1),
                                    (len, None, _) => format!("{len} found"),
                                };
                                ui.label(RichText::new(msg).size(12.5).color(p.muted));
                                ui.add_space(4.0);
                            }
                        } else if !focused {
                            Frame::new().stroke(Stroke::new(1.0, p.border)).corner_radius(5).inner_margin(Margin::symmetric(6, 2)).show(ui, |ui| {
                                ui.label(RichText::new("Ctrl F").font(theme::mono(11.0)).color(p.muted));
                            });
                            ui.add_space(4.0);
                        }

                        ui.with_layout(UiLayout::left_to_right(Align::Center), |ui| {
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut self.search.query)
                                    .id(find_id)
                                    .frame(Frame::NONE)
                                    .background_color(Color32::TRANSPARENT)
                                    .hint_text("Find in document…")
                                    .font(FontId::proportional(14.0))
                                    // As tall as the buttons, text in the middle, so it lines up with the icon.
                                    .min_size(vec2(0.0, 32.0))
                                    .align(Align2::LEFT_CENTER)
                                    .desired_width(ui.available_width()),
                            );
                            if std::mem::take(&mut self.search.focus) {
                                r.request_focus();
                            }
                            r.context_menu(|ui| {
                                let clip = arboard::Clipboard::new().and_then(|mut c| c.get_text()).unwrap_or_default();
                                let clip = clip.split_whitespace().collect::<Vec<_>>().join(" ");
                                if ui.add_enabled(!clip.is_empty(), egui::Button::new("Paste")).clicked() {
                                    paste_into(ui.ctx(), find_id, &mut self.search.query, &clip);
                                    ui.close();
                                }
                                if ui.add_enabled(!self.search.query.is_empty(), egui::Button::new("Clear")).clicked() {
                                    self.search.query.clear();
                                    ui.memory_mut(|m| m.request_focus(find_id));
                                    ui.close();
                                }
                            });
                            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                                acts.push(Act::FindNext(!ui.input(|i| i.modifiers.shift)));
                                r.request_focus();
                            }
                        });
                    });
                });
            });
    }

    /// Shown when no document is open.
    fn welcome(&self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let p = self.pal;
        let recent = self.recent_files(5);
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.5 - 170.0 - recent.len() as f32 * 18.0).max(24.0));
            let (r, _) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::hover());
            ui.painter().rect_filled(r, 14.0, p.accent);
            theme::paint_icon(ui.painter(), r.shrink(13.0), Icon::File, Color32::WHITE);
            ui.add_space(14.0);
            ui.label(RichText::new("ProtoPDF").font(theme::medium(24.0)).color(p.text));
            ui.add_space(18.0);
            let open = egui::Button::new(RichText::new("Open a PDF").font(theme::medium(15.0)).color(p.on_primary))
                .fill(p.primary)
                .stroke(Stroke::NONE)
                .corner_radius(10)
                .min_size(vec2(180.0, 40.0));
            if ui.add(open).clicked() {
                acts.push(Act::Open);
            }
            ui.add_space(8.0);
            ui.label(RichText::new("or drop a file on this window").size(13.0).color(p.muted));
            if !recent.is_empty() {
                ui.add_space(28.0);
                ui.label(RichText::new("Recent").size(13.0).color(p.muted));
                ui.add_space(4.0);
                let w = 320.0f32.min(ui.available_width());
                ui.allocate_ui(vec2(w, recent.len() as f32 * 36.0), |ui| {
                    ui.set_width(w);
                    for r in &recent {
                        let name = r.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        if theme::list_row(ui, &p, &name, false).on_hover_text(r.display().to_string()).clicked() {
                            acts.push(Act::OpenPath(r.clone()));
                        }
                    }
                });
            }
        });
    }

    fn pages(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let frame = egui::Frame::NONE.fill(self.pal.canvas);
        if self.doc.as_ref().is_none_or(|d| d.sizes.is_empty()) {
            let empty = self.doc.is_none();
            egui::CentralPanel::default().frame(egui::Frame::NONE.fill(self.pal.bg)).show(ui, |ui| {
                if empty {
                    self.welcome(ui, acts);
                } else {
                    ui.centered_and_justified(|ui| ui.spinner());
                }
            });
            return;
        }

        let view = egui::CentralPanel::default().frame(frame).show(ui, |ui| {
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
            self.update_sig_preview(&ctx, self.zoom * PT_TO_PX * ppp);
            let sig_tex = self.sig_preview.as_ref().and_then(|(_, t)| t.as_ref()).map(|t| t.id());
            let doc = self.doc.as_mut().unwrap();
            let lay = layout(&doc.sizes, self.zoom);
            let n = doc.sizes.len();

            if loaded {
                if let Some(p) = self.jump.take() {
                    let p = p.min(n - 1);
                    let offset = std::mem::take(&mut self.jump_offset) * lay.ppt;
                    self.set_scroll = Some(vec2(self.scroll.x, (lay.tops[p] + offset - GAP * 0.5).max(0.0)));
                }
            }
            let mut area = ScrollArea::both().auto_shrink(false).id_salt("pages");
            if let Some(s) = self.set_scroll.take() {
                area = area.scroll_offset(s.max(Vec2::ZERO));
            }

            let tool = self.tool;
            let sel = &mut self.sel;
            let draft = &mut self.draft;
            let sign_draft = &mut self.sign_draft;
            let note_draft = &mut self.note_draft;
            let note_open = &mut self.note_open;
            let info = self.info.clone();
            let form_edits = &mut self.form_edits;
            let active_field = &mut self.active_field;
            let focus_field = &mut self.focus_field;
            let choice_open = &mut self.choice_open;
            self.hover = None;
            let hover = &mut self.hover;
            let search = &self.search;
            let jobs = &mut self.jobs_view;
            let mut cur_page = self.cur_page;
            let snap = |v: f32| (v * ppp).round() / ppp;
            // Screen area of the selected text, for the Copy button next to it.
            let mut sel_box: Option<Rect> = None;

            let out = area.show_viewport(ui, |ui, viewport| {
                let total = vec2(lay.w.max(viewport.width()), (lay.h + BOTTOM_PAD).max(viewport.height()));
                let (rect, resp) = ui.allocate_exact_size(total, Sense::click_and_drag());
                let painter = ui.painter().clone();
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
                                sel_box = Some(sel_box.map_or(hr, |b| b.union(hr)));
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

                    if let Some(d) = note_draft.as_ref().filter(|d| d.page == i) {
                        let at = Rect::from_min_size(r.min + vec2(d.x, d.y) * lay.ppt, vec2(18.0, 18.0) * lay.ppt);
                        painter.rect_filled(at, 2.0, Color32::from_rgb(255, 212, 41));
                        painter.rect_stroke(at, 2.0, Stroke::new(1.0, Color32::from_rgb(128, 92, 0)), StrokeKind::Inside);
                    }

                    // Links, comments and form fields lie on top of the page picture.
                    if let Some(inf) = info.as_deref().filter(|_| tool == Tool::Select) {
                        if let Some(pi) = inf.pages.get(i) {
                            let on_screen = |q: [f32; 4]| Rect::from_min_max(r.min + vec2(q[0], q[1]) * lay.ppt, r.min + vec2(q[2], q[3]) * lay.ppt);
                            let base = ui.scope_id().with(("page", i));
                            for (k, link) in pi.links.iter().enumerate() {
                                let lr = ui.interact(on_screen(link.rect), base.with(("link", k)), Sense::click());
                                if lr.hovered() {
                                    ctx.set_cursor_icon(CursorIcon::PointingHand);
                                    *hover = Some(match &link.target {
                                        Target::Url(url) => url.clone(),
                                        Target::Page(p, _) => format!("Go to page {}", p + 1),
                                    });
                                }
                                if lr.clicked() {
                                    acts.push(match &link.target {
                                        Target::Url(url) => Act::OpenUrl(url.clone()),
                                        Target::Page(p, y) => Act::Goto(*p, *y),
                                    });
                                }
                            }
                            for (k, note) in pi.notes.iter().enumerate() {
                                let mut nr = ui.interact(on_screen(note.rect), base.with(("note", k)), Sense::click());
                                if nr.hovered() && !note.highlight {
                                    ctx.set_cursor_icon(CursorIcon::PointingHand);
                                }
                                if !note.text.is_empty() {
                                    nr = nr.on_hover_text(note.text.as_str());
                                }
                                if nr.clicked() {
                                    *note_open = Some((i, k, nr.rect.right_top() + vec2(8.0, 0.0)));
                                }
                            }

                            let tint = Color32::from_rgba_unmultiplied(60, 120, 255, 34);
                            let paper = Color32::from_rgb(233, 240, 255);
                            let ink = Color32::BLACK;
                            for w in &pi.widgets {
                                let wr = on_screen(w.rect);
                                let wid = base.with(("field", w.id));
                                let edited = form_edits.get(&w.field).cloned();
                                match &w.kind {
                                    WidgetKind::Text { value, multiline, max_len, size } => {
                                        let cur = match &edited {
                                            Some(FormValue::Text(t)) => t.clone(),
                                            _ => value.clone(),
                                        };
                                        let pts = if *size > 0.0 {
                                            *size
                                        } else if *multiline {
                                            10.0
                                        } else {
                                            ((w.rect[3] - w.rect[1]) * 0.65).clamp(4.0, 12.0)
                                        };
                                        let font = FontId::proportional((pts * lay.ppt).max(4.0));
                                        if *active_field == Some(w.id) {
                                            painter.rect_filled(wr, 0.0, paper);
                                            let mut text = cur.clone();
                                            let mut edit = if *multiline { egui::TextEdit::multiline(&mut text) } else { egui::TextEdit::singleline(&mut text) };
                                            edit = edit
                                                .id(wid)
                                                .frame(egui::Frame::NONE)
                                                .font(font)
                                                .text_color(ink)
                                                .margin(egui::Margin::symmetric(2, 0))
                                                .desired_width(wr.width());
                                            if let Some(limit) = max_len {
                                                edit = edit.char_limit(*limit);
                                            }
                                            let tr = ui.put(wr, edit);
                                            if std::mem::take(focus_field) {
                                                tr.request_focus();
                                            }
                                            if text != cur {
                                                if text == *value {
                                                    form_edits.remove(&w.field);
                                                } else {
                                                    form_edits.insert(w.field, FormValue::Text(text));
                                                }
                                            }
                                            if tr.lost_focus() {
                                                // Tab moves on to the next text field.
                                                let tab = ui.input(|i| i.key_pressed(Key::Tab) && !i.modifiers.shift);
                                                *active_field = if tab { inf.next_text_widget(w.id) } else { None };
                                                *focus_field = active_field.is_some();
                                            }
                                        } else {
                                            let hit = ui.interact(wr, wid.with("hit"), Sense::click());
                                            if edited.is_some() {
                                                painter.rect_filled(wr, 0.0, paper);
                                                let clipped = painter.with_clip_rect(wr);
                                                if *multiline {
                                                    clipped.text(wr.min + vec2(2.0, 1.0), Align2::LEFT_TOP, &cur, font, ink);
                                                } else {
                                                    clipped.text(pos2(wr.left() + 2.0, wr.center().y), Align2::LEFT_CENTER, &cur, font, ink);
                                                }
                                            } else {
                                                painter.rect_filled(wr, 0.0, tint);
                                            }
                                            if hit.hovered() {
                                                ctx.set_cursor_icon(CursorIcon::Text);
                                                *hover = Some(format!("Form field: {}", w.name));
                                            }
                                            if hit.clicked() {
                                                *active_field = Some(w.id);
                                                *focus_field = true;
                                            }
                                        }
                                    }
                                    WidgetKind::Check { on, checked } => {
                                        let state = match &edited {
                                            Some(FormValue::State(s)) => s == on,
                                            _ => *checked,
                                        };
                                        let hit = ui.interact(wr, wid, Sense::click());
                                        if edited.is_some() {
                                            painter.rect_filled(wr, 0.0, paper);
                                            painter.rect_stroke(wr, 0.0, Stroke::new(1.0, Color32::from_gray(80)), StrokeKind::Inside);
                                            if state {
                                                let b = wr.shrink(wr.width() * 0.22);
                                                let pen = Stroke::new((wr.width() * 0.13).max(1.5), ink);
                                                let knee = pos2(b.left() + b.width() * 0.36, b.bottom());
                                                painter.line_segment([pos2(b.left(), b.center().y), knee], pen);
                                                painter.line_segment([knee, b.right_top()], pen);
                                            }
                                        } else {
                                            painter.rect_filled(wr, 0.0, tint);
                                        }
                                        if hit.hovered() {
                                            ctx.set_cursor_icon(CursorIcon::PointingHand);
                                        }
                                        if hit.clicked() {
                                            if !state == *checked {
                                                form_edits.remove(&w.field);
                                            } else {
                                                let next = if state { "Off".to_string() } else { on.clone() };
                                                form_edits.insert(w.field, FormValue::State(next));
                                            }
                                        }
                                    }
                                    WidgetKind::Radio { on, checked } => {
                                        let chosen = match &edited {
                                            Some(FormValue::State(s)) => s == on,
                                            _ => *checked,
                                        };
                                        let hit = ui.interact(wr, wid, Sense::click());
                                        let radius = wr.width().min(wr.height()) * 0.5;
                                        if edited.is_some() {
                                            painter.circle_filled(wr.center(), radius, paper);
                                            painter.circle_stroke(wr.center(), radius - 0.5, Stroke::new(1.0, Color32::from_gray(80)));
                                            if chosen {
                                                painter.circle_filled(wr.center(), radius * 0.45, ink);
                                            }
                                        } else {
                                            painter.circle_filled(wr.center(), radius, tint);
                                        }
                                        if hit.hovered() {
                                            ctx.set_cursor_icon(CursorIcon::PointingHand);
                                        }
                                        if hit.clicked() && !chosen {
                                            form_edits.insert(w.field, FormValue::State(on.clone()));
                                        }
                                    }
                                    WidgetKind::Choice { value, options } => {
                                        let cur = match &edited {
                                            Some(FormValue::Text(t)) => t.clone(),
                                            _ => value.clone(),
                                        };
                                        let hit = ui.interact(wr, wid, Sense::click());
                                        if edited.is_some() {
                                            let shown = options.iter().find(|(stored, _)| *stored == cur).map_or(cur.as_str(), |(_, label)| label.as_str());
                                            painter.rect_filled(wr, 0.0, paper);
                                            let font = FontId::proportional((((w.rect[3] - w.rect[1]) * 0.6).clamp(4.0, 12.0) * lay.ppt).max(4.0));
                                            painter.with_clip_rect(wr).text(pos2(wr.left() + 2.0, wr.center().y), Align2::LEFT_CENTER, shown, font, ink);
                                        } else {
                                            painter.rect_filled(wr, 0.0, tint);
                                        }
                                        if hit.hovered() {
                                            ctx.set_cursor_icon(CursorIcon::PointingHand);
                                        }
                                        if hit.clicked() {
                                            *choice_open = Some((w.id, wr, frame_no));
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Where the new signature will sit.
                    if let Some(d) = sign_draft.as_ref().filter(|d| d.page == i && d.visible) {
                        let box_rect = Rect::from_min_size(r.min + vec2(d.x, d.y) * lay.ppt, vec2(SIG_W, SIG_H) * lay.ppt);
                        let blue = Color32::from_rgb(33, 97, 184);
                        painter.rect_filled(box_rect, 0.0, Color32::from_rgba_unmultiplied(33, 97, 184, 14));
                        match sig_tex {
                            // Exactly what the signed file will show.
                            Some(tex) => {
                                painter.image(tex, box_rect, uv, Color32::WHITE);
                            }
                            None => {
                                painter.rect_stroke(box_rect, 0.0, Stroke::new(1.5, blue), StrokeKind::Inside);
                                painter.text(box_rect.center(), Align2::CENTER_CENTER, "Signature", FontId::proportional(11.0 * lay.ppt), blue);
                            }
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
                    Tool::AddNote => {
                        if resp.hovered() {
                            ctx.set_cursor_icon(CursorIcon::Crosshair);
                        }
                        if resp.clicked() {
                            if let Some(pos) = resp.interact_pointer_pos() {
                                if let Some(i) = locate(pos) {
                                    let p = in_page(i, pos);
                                    *note_draft = Some(NoteDraft { page: i, x: p.x, y: p.y, text: String::new(), focus: true, screen: pos });
                                }
                            }
                        }
                    }
                    Tool::PlaceSig => {
                        if resp.hovered() {
                            ctx.set_cursor_icon(CursorIcon::Crosshair);
                        }
                        if resp.clicked() {
                            if let Some(pos) = resp.interact_pointer_pos() {
                                if let Some(i) = locate(pos) {
                                    // Keep the whole box on the page.
                                    let p = in_page(i, pos);
                                    let x = p.x.min(doc.sizes[i].0 - SIG_W - 2.0).max(2.0);
                                    let y = p.y.min(doc.sizes[i].1 - SIG_H - 2.0).max(2.0);
                                    match sign_draft.as_mut() {
                                        Some(d) => (d.page, d.x, d.y) = (i, x, y),
                                        None => {
                                            *sign_draft = Some(SignDraft {
                                                page: i,
                                                x,
                                                y,
                                                visible: true,
                                                reason: String::new(),
                                                location: String::new(),
                                                screen: pos,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
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

            // Selected text gets a Copy button just under it, once the mouse is let go.
            let dragging = ctx.input(|i| i.pointer.primary_down());
            if let Some(b) = sel_box.filter(|b| !dragging && out.inner_rect.intersects(*b)) {
                let size = vec2(76.0, 30.0);
                let below = b.bottom() + 6.0 + size.y <= out.inner_rect.bottom() - FIND_BAR_ROOM;
                let y = if below { b.bottom() + 6.0 } else { b.top() - 6.0 - size.y };
                let x = b.center().x.clamp(out.inner_rect.left() + size.x * 0.5 + 4.0, out.inner_rect.right() - size.x * 0.5 - 4.0);
                let p = self.pal;
                egui::Area::new(egui::Id::unique("sel_copy"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(pos2(x - size.x * 0.5, y.max(out.inner_rect.top() + 4.0)))
                    .show(&ctx, |ui| {
                        Frame::new()
                            .fill(p.raised)
                            .stroke(Stroke::new(1.0, p.border))
                            .corner_radius(8)
                            .inner_margin(Margin::same(3))
                            .shadow(Shadow { offset: [0, 4], blur: 12, spread: 0, color: p.shadow })
                            .show(ui, |ui| {
                                ui.spacing_mut().button_padding = vec2(16.0, 4.0);
                                let copy = egui::Button::new(RichText::new("Copy").size(13.5).color(p.text)).frame(false);
                                if ui.add(copy).on_hover_text("Copy the selected text (Ctrl+C)").clicked() {
                                    acts.push(Act::Copy);
                                }
                            });
                    });
            }

            // The list of an open drop-down field.
            if let Some((wid, at, opened)) = self.choice_open {
                let widget = self.info.as_ref().and_then(|inf| inf.widget(wid).map(|w| (w.field, &w.kind)));
                let mut close = widget.is_none();
                if let Some((field, WidgetKind::Choice { value, options })) = widget {
                    let cur = match self.form_edits.get(&field) {
                        Some(FormValue::Text(t)) => t.clone(),
                        _ => value.clone(),
                    };
                    let mut picked = None;
                    let area = egui::Area::new(egui::Id::unique("choice_list")).order(egui::Order::Foreground).fixed_pos(at.left_bottom()).show(&ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_min_width(at.width().max(120.0));
                            ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                                for (stored, label) in options {
                                    if ui.selectable_label(*stored == cur, label.as_str()).clicked() {
                                        picked = Some(stored.clone());
                                    }
                                }
                            });
                        });
                    });
                    if let Some(stored) = picked {
                        if stored == *value {
                            self.form_edits.remove(&field);
                        } else {
                            self.form_edits.insert(field, FormValue::Text(stored));
                        }
                        close = true;
                    } else if self.frame > opened && ctx.input(|i| i.pointer.any_click()) && !area.response.contains_pointer() {
                        close = true;
                    }
                }
                if close {
                    self.choice_open = None;
                }
            }
        });
        let ctx = ui.ctx().clone();
        self.overlay(&ctx, view.response.rect, acts);
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

        if let Some(setup) = self.print_setup.as_mut() {
            match setup.show(ctx) {
                Some(Some(job)) => acts.push(Act::PrintGo(job)),
                Some(None) => self.print_setup = None,
                None => {}
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

        if let Some(d) = self.note_draft.as_mut() {
            let mut open = true;
            let mut done = false;
            egui::Window::new("Add a note").collapsible(false).resizable(false).open(&mut open).default_pos(d.screen + vec2(24.0, 40.0)).show(ctx, |ui| {
                let r = ui.add(egui::TextEdit::multiline(&mut d.text).desired_rows(4).desired_width(280.0));
                if std::mem::take(&mut d.focus) {
                    r.request_focus();
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(!d.text.trim().is_empty(), egui::Button::new("Add note")).clicked() {
                        done = true;
                    }
                    ui.label("Ctrl+Enter");
                });
                if ui.input(|i| i.modifiers.command && i.key_pressed(Key::Enter)) && !d.text.trim().is_empty() {
                    done = true;
                }
            });
            if done {
                acts.push(Act::AddNote);
            } else if !open {
                self.note_draft = None;
                self.tool = Tool::Select;
            }
        }

        if let Some((page, k, at)) = self.note_open {
            let note = self.info.as_ref().and_then(|inf| inf.pages.get(page)).and_then(|p| p.notes.get(k));
            match note {
                Some(note) => {
                    let mut open = true;
                    let title = if note.highlight { "Highlight" } else { "Note" };
                    egui::Window::new(title).id(egui::Id::unique("note_shown")).collapsible(false).resizable(false).open(&mut open).fixed_pos(at).show(ctx, |ui| {
                        ui.set_max_width(300.0);
                        if !note.author.is_empty() {
                            ui.label(egui::RichText::new(&note.author).strong());
                        }
                        if !note.text.is_empty() {
                            ui.label(&note.text);
                            ui.add_space(4.0);
                        }
                        let label = if note.highlight { "Remove highlight" } else { "Delete note" };
                        if ui.add_enabled(self.edit_rx.is_none(), egui::Button::new(label)).clicked() {
                            acts.push(Act::DeleteAnnot(page, note.id));
                        }
                    });
                    if !open {
                        self.note_open = None;
                    }
                }
                None => self.note_open = None,
            }
        }

        #[cfg(not(windows))]
        let p12 = &mut self.p12;
        #[cfg(windows)]
        let cert_name = self.sign_cert.as_ref().map(|(_, n)| n.clone());
        if let Some(d) = self.sign_draft.as_mut() {
            let mut open = true;
            egui::Window::new("Sign document")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .default_pos(d.screen + vec2(30.0, 90.0))
                .show(ctx, |ui| {
                    ui.set_width(300.0);
                    ui.checkbox(&mut d.visible, "Show the signature on the page");
                    if d.visible {
                        ui.label("Click on the page to move it.");
                    }
                    ui.add_space(4.0);
                    egui::Grid::new("sign_fields").num_columns(2).show(ui, |ui| {
                        ui.label("Reason");
                        ui.add(egui::TextEdit::singleline(&mut d.reason).hint_text("optional").desired_width(220.0));
                        ui.end_row();
                        ui.label("Place");
                        ui.add(egui::TextEdit::singleline(&mut d.location).hint_text("optional").desired_width(220.0));
                        ui.end_row();
                    });
                    ui.add_space(6.0);
                    #[cfg(windows)]
                    {
                        egui::Grid::new("sign_cert").num_columns(2).show(ui, |ui| {
                            ui.label("Certificate");
                            ui.horizontal(|ui| {
                                ui.add_sized([150.0, 18.0], egui::Label::new(cert_name.as_deref().unwrap_or("none chosen")).truncate());
                                if ui.button("Choose...").clicked() {
                                    acts.push(Act::PickCert);
                                }
                            });
                            ui.end_row();
                        });
                        ui.add_space(6.0);
                        if ui.button("Sign...").clicked() {
                            acts.push(Act::SignGo);
                        }
                    }
                    #[cfg(not(windows))]
                    {
                        egui::Grid::new("sign_cert").num_columns(2).show(ui, |ui| {
                            ui.label("Certificate");
                            ui.horizontal(|ui| {
                                let shown = p12.0.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string());
                                ui.add_sized([150.0, 18.0], egui::Label::new(shown.as_deref().unwrap_or("none chosen")).truncate());
                                if ui.button("Choose...").clicked() {
                                    let mut dlg = rfd::FileDialog::new().add_filter("Certificate file", &["p12", "pfx", "P12", "PFX"]).set_title("Certificate file");
                                    if let Some(dir) = p12.0.as_ref().and_then(|p| p.parent()) {
                                        dlg = dlg.set_directory(dir);
                                    }
                                    if let Some(f) = dlg.pick_file() {
                                        p12.0 = Some(f);
                                    }
                                }
                            });
                            ui.end_row();
                            ui.label("Password");
                            ui.add(egui::TextEdit::singleline(&mut p12.1).password(true).desired_width(220.0));
                            ui.end_row();
                        });
                        ui.add_space(6.0);
                        if ui.add_enabled(p12.0.is_some(), egui::Button::new("Sign...")).clicked() {
                            acts.push(Act::SignGo);
                        }
                        ui.label("Signs with a .p12 / .pfx file that holds your certificate and private key.");
                    }
                    ui.label("The signed document is saved as a new file.");
                });
            if !open {
                self.sign_draft = None;
                self.tool = Tool::Select;
            }
        }

        if self.show_sigs && !self.sigs.is_empty() {
            let mut open = true;
            egui::Window::new("Signatures").collapsible(false).resizable(false).open(&mut open).default_pos(pos2(90.0, 150.0)).show(ctx, |ui| {
                ui.set_width(430.0);
                for (i, s) in self.sigs.iter().enumerate() {
                    if i > 0 {
                        ui.separator();
                    }
                    let color = match s.level() {
                        Level::Good => Color32::from_rgb(60, 170, 95),
                        Level::Warn => Color32::from_rgb(214, 160, 40),
                        Level::Bad => Color32::from_rgb(225, 80, 80),
                    };
                    let title = if s.is_timestamp {
                        "Document timestamp".to_string()
                    } else if s.org.is_empty() || s.org == s.signer {
                        s.signer.clone()
                    } else {
                        format!("{} ({})", s.signer, s.org)
                    };
                    ui.label(egui::RichText::new(title).strong().size(15.0));
                    ui.colored_label(color, &s.intact_msg);
                    if !s.trust_msg.is_empty() {
                        ui.colored_label(if s.trusted { color } else { Color32::from_rgb(214, 160, 40) }, &s.trust_msg);
                    }
                    if !s.is_timestamp && !s.signed_at.is_empty() {
                        ui.label(format!("Signed: {} (time from the signer's computer)", s.signed_at));
                    }
                    if let Some(t) = &s.timestamp {
                        ui.label(format!("Timestamp: {t} (confirmed by a timestamp authority)"));
                    }
                    if !s.issuer.is_empty() {
                        ui.label(format!("Certificate issued by {}, valid until {}", s.issuer, s.cert_until));
                    }
                    if !s.reason.is_empty() {
                        ui.label(format!("Reason: {}", s.reason));
                    }
                    if !s.location.is_empty() {
                        ui.label(format!("Place: {}", s.location));
                    }
                }
            });
            if !open {
                self.show_sigs = false;
            }
        }

        if self.pending_edit.is_some() {
            egui::Modal::new(egui::Id::unique("edit_signed")).show(ctx, |ui| {
                ui.set_width(360.0);
                ui.heading("This document is signed");
                ui.label("Changing it makes the digital signatures invalid. The original file stays as it is until you save.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Change it anyway").clicked() {
                        acts.push(Act::EditSigned(true));
                    }
                    if ui.button("Cancel").clicked() {
                        acts.push(Act::EditSigned(false));
                    }
                });
            });
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
            Some(d) => format!("{}{} - ProtoPDF", d.name(), if d.dirty() || !self.form_edits.is_empty() { " *" } else { "" }),
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
            Some(d) => d.loaded && !d.pool.busy() && !self.busy() && self.sig_rx.is_none() && self.info_rx.is_none() && self.queued.is_empty(),
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
            "showsigs" => Act::ShowSigs,
            "highlight" => Act::Highlight,
            "commit" => Act::CommitForm,
            "save" => Act::Save,
            "back" => Act::Back,
            "contents" => {
                self.show_thumbs = true;
                self.side_tab = SideTab::Contents;
                return;
            }
            "comments" => {
                self.show_thumbs = true;
                self.side_tab = SideTab::Comments;
                return;
            }
            "find" => {
                self.search.query = arg.to_string();
                return;
            }
            "theme" => Act::ToggleTheme,
            // goto:<n> follows the n-th entry of the contents; link:<n> the n-th link on this page.
            "goto" | "link" => {
                let Some(info) = self.info.clone() else { return };
                let k = arg.parse::<usize>().unwrap_or(1).saturating_sub(1);
                let target = if name == "goto" {
                    info.outline.get(k).and_then(|o| o.page.map(|p| (p, o.y)))
                } else {
                    match info.pages.get(self.cur_page).and_then(|p| p.links.get(k)).map(|l| &l.target) {
                        Some(Target::Page(p, y)) => Some((*p, *y)),
                        Some(Target::Url(u)) => {
                            self.status = format!("link to {u}");
                            None
                        }
                        None => None,
                    }
                };
                match target {
                    Some((page, y)) => Act::Goto(page, y),
                    None => return,
                }
            }
            // fill:<field name>=<value>. Checkboxes take on/off, radio buttons and lists the stored value.
            "fill" | "type" => {
                let Some(info) = self.info.clone() else { return };
                let (field, value) = arg.split_once('=').unwrap_or((arg, ""));
                for w in info.pages.iter().flat_map(|p| p.widgets.iter()).filter(|w| w.name == field) {
                    match &w.kind {
                        WidgetKind::Text { .. } | WidgetKind::Choice { .. } => {
                            self.form_edits.insert(w.field, FormValue::Text(value.replace("\\n", "\n")));
                            if name == "type" {
                                self.active_field = Some(w.id);
                                self.focus_field = true;
                            }
                        }
                        WidgetKind::Check { on, .. } => {
                            let state = if value == "on" { on.clone() } else { "Off".to_string() };
                            self.form_edits.insert(w.field, FormValue::State(state));
                        }
                        WidgetKind::Radio { .. } => {
                            self.form_edits.insert(w.field, FormValue::State(value.to_string()));
                        }
                    }
                }
                return;
            }
            "note" => {
                let mut it = arg.splitn(3, ',');
                let x = it.next().and_then(|v| v.parse().ok()).unwrap_or(40.0);
                let y = it.next().and_then(|v| v.parse().ok()).unwrap_or(40.0);
                self.note_draft = Some(NoteDraft { page: self.cur_page, x, y, text: it.next().unwrap_or("").to_string(), focus: false, screen: pos2(300.0, 300.0) });
                Act::AddNote
            }
            "shownote" => {
                self.note_open = Some((self.cur_page, arg.parse::<usize>().unwrap_or(1).saturating_sub(1), pos2(420.0, 220.0)));
                return;
            }
            "editsigned" => Act::EditSigned(true),
            // Opens the print dialog.
            "print" => Act::Print,
            "placesig" => {
                let mut it = arg.split(',');
                let x = it.next().and_then(|v| v.parse().ok()).unwrap_or(40.0);
                let y = it.next().and_then(|v| v.parse().ok()).unwrap_or(40.0);
                self.tool = Tool::PlaceSig;
                self.sign_draft = Some(SignDraft {
                    page: self.cur_page,
                    x,
                    y,
                    visible: true,
                    reason: String::new(),
                    location: String::new(),
                    screen: pos2(420.0, 200.0),
                });
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

/// Put `text` into a one-line text box at its cursor, replacing what is selected.
fn paste_into(ctx: &egui::Context, id: egui::Id, value: &mut String, text: &str) {
    use egui::text::{CCursor, CCursorRange};
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    let len = value.chars().count();
    let (a, b) = state.cursor.char_range().map_or((len, len), |r| {
        let (x, y) = (usize::from(r.primary.index).min(len), usize::from(r.secondary.index).min(len));
        (x.min(y), x.max(y))
    });
    let start = value.char_indices().nth(a).map_or(value.len(), |(i, _)| i);
    let end = value.char_indices().nth(b).map_or(value.len(), |(i, _)| i);
    value.replace_range(start..end, text);
    let at = a + text.chars().count();
    state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(at))));
    state.store(ctx, id);
    ctx.memory_mut(|m| m.request_focus(id));
}

/// "A4", "Letter" and friends, when the page is one of them (either way up).
fn paper_name(w: f32, h: f32) -> Option<&'static str> {
    let (a, b) = (w.min(h), w.max(h));
    const PAPERS: [(&str, f32, f32); 6] = [
        ("A4", 595.3, 841.9),
        ("A3", 841.9, 1190.6),
        ("A5", 419.5, 595.3),
        ("Letter", 612.0, 792.0),
        ("Legal", 612.0, 1008.0),
        ("B5", 498.9, 708.7),
    ];
    PAPERS.iter().find(|(_, pw, ph)| (a - pw).abs() < 3.0 && (b - ph).abs() < 3.0).map(|p| p.0)
}

fn human_size(bytes: usize) -> String {
    let b = bytes as f64;
    if b < 1024.0 {
        format!("{bytes} B")
    } else if b < 1024.0 * 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    }
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
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.frame += 1;
        // Once the window is up, and after every theme change, a few frames later.
        if self.frame == self.title_bar_at {
            theme::sync_title_bar(frame, !self.settings.light);
        }
        self.poll(&ctx);

        let mut acts = Vec::new();
        // Steps that were waiting for the form values to be written into the document.
        if !self.queued.is_empty() && !self.busy() && self.doc.as_ref().is_some_and(|d| d.loaded) {
            acts.append(&mut self.queued);
        }
        self.shortcuts(&ctx, &mut acts);
        // Narrow windows drop the side panels before squeezing the pages.
        let width = ctx.content_rect().width();
        let side_open = self.show_thumbs && width >= 720.0;
        let has_pages = self.doc.as_ref().is_some_and(|d| !d.sizes.is_empty());
        let panel_open = self.show_panel && has_pages && width >= 1020.0 - if side_open { 0.0 } else { SIDEBAR_W };
        if side_open {
            self.side_panel(ui, &mut acts);
        }
        self.top_bar(ui, &mut acts, side_open, panel_open);
        if panel_open {
            self.info_panel(ui, &mut acts);
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
