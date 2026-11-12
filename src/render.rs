//! Background rendering: a small pool of threads that turn pages into bitmaps
//! and extract page text. The UI replaces the wanted-job list every frame, so
//! work that scrolled out of view is dropped without ever being started.

use crate::text::{PageText, TGlyph};
use hayro::hayro_interpret::font::{FontData, FontQuery, Glyph, GlyphRun, StandardFont};
use hayro::hayro_interpret::hayro_cmap::BfString;
use hayro::hayro_interpret::util::TransformExt;
use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps,
    InterpreterCache, InterpreterSettings, SoftMask, interpret_page,
};
use hayro::hayro_syntax::page::Page;
use hayro::hayro_syntax::{LoadPdfError, Pdf};
use hayro::kurbo::{Affine, BezPath, Point, Rect};
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings};
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    /// Render the page so that it is this many pixels wide.
    Render(u32),
    Text,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Job {
    pub page: usize,
    pub kind: Kind,
}

pub enum OpenError {
    NeedPassword,
    Invalid,
}

pub enum Out {
    /// Page sizes in points, as displayed (rotation applied).
    Opened(Vec<(f32, f32)>),
    Failed(OpenError),
    Image { page: usize, width: u32, w: usize, h: usize, rgba: Vec<u8> },
    Text { page: usize, text: PageText },
}

#[derive(Default)]
struct State {
    jobs: Vec<Job>,
    running: Vec<Job>,
    closed: bool,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}

pub struct Pool {
    shared: Arc<Shared>,
    pub rx: Receiver<Out>,
}

impl Pool {
    pub fn new(
        bytes: Arc<Vec<u8>>,
        password: String,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let shared = Arc::new(Shared { state: Mutex::new(State::default()), cv: Condvar::new() });
        let (tx, rx) = channel();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(1))
            .unwrap_or(2)
            .clamp(1, 4);
        for i in 0..threads {
            let shared = shared.clone();
            let tx = tx.clone();
            let bytes = bytes.clone();
            let password = password.clone();
            let wake = wake.clone();
            let _ = std::thread::Builder::new()
                .name(format!("render-{i}"))
                .stack_size(16 * 1024 * 1024)
                .spawn(move || worker(i == 0, bytes, &password, &shared, &tx, &*wake));
        }
        Self { shared, rx }
    }

    /// Replace the queue. Jobs are taken from the front, so order is priority.
    pub fn set_jobs(&self, jobs: Vec<Job>) {
        let mut st = self.shared.state.lock().unwrap();
        st.jobs = jobs;
        let State { jobs, running, .. } = &mut *st;
        jobs.retain(|j| !running.contains(j));
        let any = !jobs.is_empty();
        drop(st);
        if any {
            self.shared.cv.notify_all();
        }
    }

    pub fn busy(&self) -> bool {
        let st = self.shared.state.lock().unwrap();
        !st.jobs.is_empty() || !st.running.is_empty()
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().closed = true;
        self.shared.cv.notify_all();
    }
}

pub fn open_pdf(bytes: Arc<Vec<u8>>, password: &str) -> Result<Pdf, OpenError> {
    Pdf::new_with_password(bytes, password).map_err(|e| match e {
        LoadPdfError::Decryption(_) => OpenError::NeedPassword,
        LoadPdfError::Invalid => OpenError::Invalid,
    })
}

fn worker(
    lead: bool,
    bytes: Arc<Vec<u8>>,
    password: &str,
    shared: &Shared,
    tx: &Sender<Out>,
    wake: &(dyn Fn() + Send + Sync),
) {
    let pdf = match catch_unwind(AssertUnwindSafe(|| open_pdf(bytes, password))) {
        Ok(Ok(pdf)) => pdf,
        Ok(Err(e)) => {
            if lead {
                let _ = tx.send(Out::Failed(e));
                wake();
            }
            return;
        }
        Err(_) => {
            if lead {
                let _ = tx.send(Out::Failed(OpenError::Invalid));
                wake();
            }
            return;
        }
    };
    let pages = pdf.pages();
    if lead {
        let sizes = pages.iter().map(|p| p.render_dimensions()).collect();
        let _ = tx.send(Out::Opened(sizes));
        wake();
    }

    let settings = interpreter_settings();
    let mut cache = RenderCache::new();
    let mut done = 0usize;

    loop {
        let job = {
            let mut st = shared.state.lock().unwrap();
            loop {
                if st.closed {
                    return;
                }
                if !st.jobs.is_empty() {
                    let job = st.jobs.remove(0);
                    st.running.push(job);
                    break job;
                }
                st = shared.cv.wait(st).unwrap();
            }
        };

        let out = pages.get(job.page).and_then(|page| {
            let res = catch_unwind(AssertUnwindSafe(|| match job.kind {
                Kind::Render(width) => {
                    let (w, h, rgba) = render_page(page, &cache, &settings, width, 8192);
                    Out::Image { page: job.page, width, w, h, rgba }
                }
                Kind::Text => Out::Text { page: job.page, text: page_text(page, &settings) },
            }));
            res.ok()
        });

        // The glyph and object caches only grow, and a panic can leave them
        // half-updated, so start fresh now and then.
        done += 1;
        if out.is_none() || done % 48 == 0 {
            cache = RenderCache::new();
        }

        let out = out.unwrap_or_else(|| match job.kind {
            // A page that cannot be drawn shows as blank instead of retrying forever.
            Kind::Render(width) => {
                Out::Image { page: job.page, width, w: 1, h: 1, rgba: vec![255, 255, 255, 255] }
            }
            Kind::Text => Out::Text { page: job.page, text: PageText::default() },
        });

        {
            let mut st = shared.state.lock().unwrap();
            st.running.retain(|j| *j != job);
            if st.closed {
                return;
            }
        }
        if tx.send(out).is_err() {
            return;
        }
        wake();
    }
}

/// Render a page `width` pixels wide (capped so neither side exceeds `max_side`).
/// Returns width, height and opaque RGBA pixels.
pub fn render_page<'a>(
    page: &'a Page<'a>,
    cache: &RenderCache<'a>,
    settings: &InterpreterSettings,
    width: u32,
    max_side: u32,
) -> (usize, usize, Vec<u8>) {
    let (pw, ph) = page.render_dimensions();
    let mut scale = width as f32 / pw.max(1.0);
    let longest = (pw.max(ph) * scale).max(1.0);
    if longest > max_side as f32 {
        scale *= max_side as f32 / longest;
    }
    // Round up a hair so the bitmap is not one pixel short of the request.
    let scale = scale * 1.0005;
    let pix = hayro::render(
        page,
        cache,
        settings,
        &RenderSettings::default(),
        &PixmapSettings { x_scale: scale, y_scale: scale, bg_color: WHITE },
    );
    let (w, h) = (pix.width() as usize, pix.height() as usize);
    (w, h, pix.data_as_u8_slice().to_vec())
}

pub fn page_text<'a>(page: &'a Page<'a>, settings: &InterpreterSettings) -> PageText {
    let (w, h) = page.render_dimensions();
    let transform = page.initial_transform(true).to_kurbo();
    let cache = InterpreterCache::new();
    let mut ctx = Context::new(
        transform,
        Rect::new(0.0, 0.0, w as f64, h as f64),
        &cache,
        page.xref(),
        settings.clone(),
    );
    let mut dev = TextDevice { glyphs: Vec::new() };
    interpret_page(page, &mut ctx, &mut dev);
    PageText::build(dev.glyphs)
}

struct TextDevice {
    glyphs: Vec<TGlyph>,
}

impl<'a> Device<'a> for TextDevice {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}

    fn draw_glyph_run(&mut self, run: &GlyphRun<'_, 'a>, props: DrawProps<'a>, _: &DrawMode) {
        for g in run.glyphs() {
            let Glyph::Outline(outline) = &**g else { continue };
            let Some(u) = g.as_unicode() else { continue };
            let s = match u {
                BfString::Char(c) => c.to_string(),
                BfString::String(s) => s,
            };
            if s.is_empty() || s.chars().all(|c| c.is_control()) {
                continue;
            }
            let adv = outline.advance_width().filter(|a| *a > 0.0).unwrap_or(500.0) as f64;
            let t: Affine = props.transform * g.transform();
            // Glyph space is 1000 units per em, y up: cover descender to ascender.
            let pts = [
                t * Point::new(0.0, -220.0),
                t * Point::new(adv, -220.0),
                t * Point::new(0.0, 780.0),
                t * Point::new(adv, 780.0),
            ];
            let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for p in pts {
                x0 = x0.min(p.x);
                y0 = y0.min(p.y);
                x1 = x1.max(p.x);
                y1 = y1.max(p.y);
            }
            if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
                continue;
            }
            self.glyphs.push(TGlyph {
                s,
                x0: x0 as f32,
                y0: y0 as f32,
                x1: x1 as f32,
                y1: y1 as f32,
            });
        }
    }
}

/// Fonts that a PDF names but does not embed are taken from the system (Arial,
/// Times New Roman and Courier New on Windows, the Liberation fonts on Linux), so
/// that Cyrillic text in such files shows correctly. The built-in fonts are the fallback.
pub fn interpreter_settings() -> InterpreterSettings {
    InterpreterSettings {
        font_resolver: Arc::new(|query| match query {
            FontQuery::Standard(s) => system_font(s).or_else(|| Some(s.get_font_data())),
            FontQuery::Fallback(f) => {
                let s = f.pick_standard_font();
                system_font(&s).or_else(|| Some(s.get_font_data()))
            }
        }),
        ..Default::default()
    }
}

fn system_font(font: &StandardFont) -> Option<(FontData, u32)> {
    // Windows names first, then fonts with the same widths found on Linux.
    let files: &'static [&'static str] = match font {
        StandardFont::Helvetica => &["arial.ttf", "LiberationSans-Regular.ttf", "Arimo-Regular.ttf"],
        StandardFont::HelveticaBold => &["arialbd.ttf", "LiberationSans-Bold.ttf", "Arimo-Bold.ttf"],
        StandardFont::HelveticaOblique => &["ariali.ttf", "LiberationSans-Italic.ttf", "Arimo-Italic.ttf"],
        StandardFont::HelveticaBoldOblique => &["arialbi.ttf", "LiberationSans-BoldItalic.ttf", "Arimo-BoldItalic.ttf"],
        StandardFont::Courier => &["cour.ttf", "LiberationMono-Regular.ttf", "Cousine-Regular.ttf"],
        StandardFont::CourierBold => &["courbd.ttf", "LiberationMono-Bold.ttf", "Cousine-Bold.ttf"],
        StandardFont::CourierOblique => &["couri.ttf", "LiberationMono-Italic.ttf", "Cousine-Italic.ttf"],
        StandardFont::CourierBoldOblique => &["courbi.ttf", "LiberationMono-BoldItalic.ttf", "Cousine-BoldItalic.ttf"],
        StandardFont::TimesRoman => &["times.ttf", "LiberationSerif-Regular.ttf", "Tinos-Regular.ttf"],
        StandardFont::TimesBold => &["timesbd.ttf", "LiberationSerif-Bold.ttf", "Tinos-Bold.ttf"],
        StandardFont::TimesItalic => &["timesi.ttf", "LiberationSerif-Italic.ttf", "Tinos-Italic.ttf"],
        StandardFont::TimesBoldItalic => &["timesbi.ttf", "LiberationSerif-BoldItalic.ttf", "Tinos-BoldItalic.ttf"],
        StandardFont::ZapfDingBats | StandardFont::Symbol => return None,
    };
    static FONTS: OnceLock<Mutex<HashMap<&'static str, Option<Arc<Vec<u8>>>>>> = OnceLock::new();
    let mut map = FONTS.get_or_init(Default::default).lock().ok()?;
    let data = map.entry(files[0]).or_insert_with(|| crate::fonts::find(files).map(Arc::new)).clone()?;
    Some((data, 0))
}
