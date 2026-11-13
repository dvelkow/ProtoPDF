//! Printing on Windows. ProtoPDF draws its own print dialog with a preview of
//! every sheet as it will come out of the printer: the Windows 11 print
//! dialog cannot show a preview for a program like this one. The printer
//! driver's own settings window (duplex, paper tray, quality) is one click
//! away. Pages are rendered to bitmaps and sent to the printer with GDI, so
//! any installed printer works (including "Microsoft Print to PDF").

use crate::render;
use eframe::egui::{self, Color32, ColorImage, Mesh, Rect, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, pos2, vec2};
use hayro::RenderCache;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDCW, CreateICW, DEVMODE_FIELD_FLAGS, DEVMODEW, DIB_RGB_COLORS, DM_COLLATE,
    DM_COLOR, DM_COPIES, DM_IN_BUFFER, DM_IN_PROMPT, DM_ORIENTATION, DM_OUT_BUFFER, DM_PAPERLENGTH, DM_PAPERSIZE,
    DM_PAPERWIDTH, DMCOLLATE_TRUE, DMCOLOR_COLOR, DMCOLOR_MONOCHROME, DMORIENT_LANDSCAPE, DMORIENT_PORTRAIT, DeleteDC,
    GetDeviceCaps, HALFTONE, HDC, HORZRES, LOGPIXELSX, LOGPIXELSY, PHYSICALHEIGHT, PHYSICALOFFSETX, PHYSICALOFFSETY,
    PHYSICALWIDTH, SRCCOPY, SetStretchBltMode, StretchDIBits, VERTRES,
};
use windows::Win32::Graphics::Printing::{
    ClosePrinter, DocumentPropertiesW, EnumPrintersW, GetDefaultPrinterW, OpenPrinterW, PRINTER_ENUM_CONNECTIONS,
    PRINTER_ENUM_LOCAL, PRINTER_HANDLE, PRINTER_INFO_4W,
};
use windows::Win32::Storage::Xps::{
    AbortDoc, DC_COLORDEVICE, DC_COPIES, DC_PAPERNAMES, DC_PAPERS, DOCINFOW, DeviceCapabilitiesW, EndDoc, EndPage,
    StartDocW, StartPage,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows::core::{HSTRING, PCWSTR, PWSTR};

/// Longest side of a preview image, in pixels.
const PREVIEW_PX: f32 = 900.0;

pub enum Progress {
    Page(usize, usize),
    Done,
    Failed(String),
}

struct PrinterDc(HDC);
// A printer device context is not tied to the thread that created it.
unsafe impl Send for PrinterDc {}

pub struct PrintJob {
    dc: PrinterDc,
    /// 0-based, inclusive.
    first: usize,
    last: usize,
    /// Turn pages whose orientation differs from the paper's.
    auto_turn: bool,
    /// How many times ProtoPDF sends the pages itself (for drivers that cannot make copies).
    copies: u32,
}

// ---------------------------------------------------------------------------
// The sheet and where a page lands on it
// ---------------------------------------------------------------------------

/// A sheet of paper as the printer sees it, in device pixels.
#[derive(Clone, Copy)]
struct Sheet {
    /// Printable area.
    res_x: f32,
    res_y: f32,
    dpi_x: f32,
    dpi_y: f32,
    /// The whole sheet, and where the printable area starts on it.
    phys_w: f32,
    phys_h: f32,
    off_x: f32,
    off_y: f32,
}

impl Sheet {
    fn of(hdc: HDC) -> Sheet {
        let cap = |i| unsafe { GetDeviceCaps(Some(hdc), i) } as f32;
        let (res_x, res_y) = (cap(HORZRES).max(1.0), cap(VERTRES).max(1.0));
        Sheet {
            res_x,
            res_y,
            dpi_x: cap(LOGPIXELSX).max(72.0),
            dpi_y: cap(LOGPIXELSY).max(72.0),
            phys_w: cap(PHYSICALWIDTH).max(res_x),
            phys_h: cap(PHYSICALHEIGHT).max(res_y),
            off_x: cap(PHYSICALOFFSETX).max(0.0),
            off_y: cap(PHYSICALOFFSETY).max(0.0),
        }
    }

    /// A4 without margins, when the printer cannot be asked.
    fn a4() -> Sheet {
        let (w, h) = (595.0, 842.0);
        Sheet { res_x: w, res_y: h, dpi_x: 72.0, dpi_y: 72.0, phys_w: w, phys_h: h, off_x: 0.0, off_y: 0.0 }
    }

    fn mm(&self) -> (f32, f32) {
        (self.phys_w / self.dpi_x * 25.4, self.phys_h / self.dpi_y * 25.4)
    }
}

/// Where a page of `pw` x `ph` points lands, in device pixels from the corner
/// of the printable area. Used both to print and to draw the preview.
struct Place {
    turn: bool,
    fit: f32,
    dx: f32,
    dy: f32,
    dw: f32,
    dh: f32,
}

fn place(pw: f32, ph: f32, s: &Sheet, auto_turn: bool) -> Place {
    // A landscape page on portrait paper (or the reverse) is turned to fill the sheet.
    let turn = auto_turn && (pw > ph) != (s.res_x / s.dpi_x > s.res_y / s.dpi_y);
    let (sw, sh) = if turn { (ph, pw) } else { (pw, ph) };
    // Size of the page in device pixels at actual size, shrunk to the printable area.
    let (full_w, full_h) = (sw / 72.0 * s.dpi_x, sh / 72.0 * s.dpi_y);
    let fit = (s.res_x / full_w).min(s.res_y / full_h).min(1.0);
    let (dw, dh) = ((full_w * fit).floor().max(1.0), (full_h * fit).floor().max(1.0));
    let (dx, dy) = (((s.res_x - dw) / 2.0).floor(), ((s.res_y - dh) / 2.0).floor());
    Place { turn, fit, dx, dy, dw, dh }
}

// ---------------------------------------------------------------------------
// Printers and their settings
// ---------------------------------------------------------------------------

/// A printer's settings (a DEVMODE with the driver's private part after it).
struct DevMode(Vec<u64>);

impl DevMode {
    fn ptr(&self) -> *const DEVMODEW {
        self.0.as_ptr().cast()
    }
    fn get(&self) -> &DEVMODEW {
        unsafe { &*self.ptr() }
    }
    fn get_mut(&mut self) -> &mut DEVMODEW {
        unsafe { &mut *self.0.as_mut_ptr().cast::<DEVMODEW>() }
    }
}

/// The printer's settings: its defaults, or `start` passed through the driver.
/// With `prompt` the driver's own settings window is shown first; `None` if
/// the user cancelled it or the driver failed.
fn devmode(printer: &str, start: Option<&DevMode>, prompt: bool) -> Option<DevMode> {
    unsafe {
        let name = HSTRING::from(printer);
        let mut h = PRINTER_HANDLE::default();
        OpenPrinterW(&name, &mut h, None).ok()?;
        let size = DocumentPropertiesW(None, h, &name, None, None, 0);
        let mut res = None;
        if size > 0 {
            let mut out = DevMode(vec![0u64; (size as usize).div_ceil(8) + 1]);
            let mut mode = DM_OUT_BUFFER.0;
            if start.is_some() {
                mode |= DM_IN_BUFFER.0;
            }
            if prompt {
                mode |= DM_IN_PROMPT.0;
            }
            let owner: Option<HWND> = prompt.then(|| GetActiveWindow());
            let r = DocumentPropertiesW(owner, h, &name, Some(out.0.as_mut_ptr().cast()), start.map(DevMode::ptr), mode);
            // IDOK
            if r == 1 {
                res = Some(out);
            }
        }
        let _ = ClosePrinter(h);
        res
    }
}

fn default_printer() -> Option<String> {
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    if !unsafe { GetDefaultPrinterW(Some(PWSTR(buf.as_mut_ptr())), &mut len) }.as_bool() {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)]))
}

/// The installed printers, the default one first.
fn printers() -> Vec<String> {
    let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
    let (mut need, mut count) = (0u32, 0u32);
    let mut list = Vec::new();
    unsafe {
        let _ = EnumPrintersW(flags, PCWSTR::null(), 4, None, &mut need, &mut count);
        if need > 0 {
            let mut buf = vec![0u64; (need as usize).div_ceil(8)];
            let bytes = std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), need as usize);
            if EnumPrintersW(flags, PCWSTR::null(), 4, Some(bytes), &mut need, &mut count).is_ok() {
                let infos = std::slice::from_raw_parts(buf.as_ptr().cast::<PRINTER_INFO_4W>(), count as usize);
                list = infos.iter().filter_map(|i| i.pPrinterName.to_string().ok()).filter(|n| !n.is_empty()).collect();
            }
        }
    }
    if let Some(d) = default_printer() {
        match list.iter().position(|p| *p == d) {
            Some(i) => {
                let p = list.remove(i);
                list.insert(0, p);
            }
            None => list.insert(0, d),
        }
    }
    list
}

fn capability(printer: &str, what: windows::Win32::Storage::Xps::PRINTER_DEVICE_CAPABILITIES, out: Option<PWSTR>, dm: Option<&DevMode>) -> i32 {
    unsafe { DeviceCapabilitiesW(&HSTRING::from(printer), PCWSTR::null(), what, out, dm.map(DevMode::ptr)) }
}

/// The paper sizes the printer offers: (DEVMODE paper number, name).
fn papers(printer: &str, dm: Option<&DevMode>) -> Vec<(i16, String)> {
    let n = capability(printer, DC_PAPERS, None, dm);
    if n <= 0 || n > 1000 {
        return Vec::new();
    }
    let mut ids = vec![0u16; n as usize];
    let mut names = vec![0u16; n as usize * 64];
    if capability(printer, DC_PAPERS, Some(PWSTR(ids.as_mut_ptr())), dm) != n
        || capability(printer, DC_PAPERNAMES, Some(PWSTR(names.as_mut_ptr())), dm) != n
    {
        return Vec::new();
    }
    ids.iter()
        .zip(names.chunks(64))
        .map(|(id, raw)| {
            let len = raw.iter().position(|c| *c == 0).unwrap_or(raw.len());
            (*id as i16, String::from_utf16_lossy(&raw[..len]).trim().to_string())
        })
        .filter(|(_, name)| !name.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// The print dialog
// ---------------------------------------------------------------------------

#[derive(PartialEq, Eq, Clone, Copy)]
enum Which {
    All,
    Current,
    Range,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Orient {
    /// Portrait paper; landscape pages are turned to fill it.
    Auto,
    Portrait,
    Landscape,
}

type Image = (usize, usize, Vec<u8>);

pub struct Setup {
    printers: Vec<String>,
    printer: usize,
    devmode: Option<DevMode>,
    papers: Vec<(i16, String)>,
    /// DEVMODE paper number; 0 when the driver uses a size of its own.
    paper: i16,
    color_device: bool,
    color: bool,
    /// Copies the driver can make itself.
    driver_copies: u32,
    orient: Orient,
    which: Which,
    from: usize,
    to: usize,
    copies: u32,
    current: usize,
    /// Page sizes in points, as rendered.
    sizes: Vec<(f32, f32)>,
    sheet: Sheet,
    /// The sheet shown in the preview, 0-based within the pages to print.
    shown: usize,
    images: HashMap<usize, Image>,
    asked: HashSet<usize>,
    tex: Option<((usize, bool), TextureHandle)>,
    want: Sender<usize>,
    got: Receiver<(usize, Image)>,
    error: Option<String>,
}

impl Setup {
    pub fn new(sizes: Vec<(f32, f32)>, current_page: usize, bytes: Arc<Vec<u8>>, password: String, ctx: &egui::Context) -> Self {
        let n = sizes.len().max(1);
        let cur = current_page.min(n - 1);
        let (want, want_rx) = channel::<usize>();
        let (got_tx, got) = channel();
        let wake = ctx.clone();
        // Preview pages are rendered on their own thread so the dialog never stalls.
        let _ = std::thread::Builder::new().name("print-preview".into()).stack_size(16 * 1024 * 1024).spawn(move || {
            let Ok(pdf) = render::open_pdf(bytes, &password) else { return };
            let pages = pdf.pages();
            let settings = render::interpreter_settings();
            let cache = RenderCache::new();
            while let Ok(i) = want_rx.recv() {
                let Some(page) = pages.get(i) else { continue };
                let (pw, ph) = page.render_dimensions();
                let width = (PREVIEW_PX * pw / pw.max(ph).max(1.0)).round().max(16.0) as u32;
                let img = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    render::render_page(page, &cache, &settings, width, PREVIEW_PX as u32 + 2)
                }))
                .unwrap_or((1, 1, vec![255; 4]));
                if got_tx.send((i, img)).is_err() {
                    return;
                }
                wake.request_repaint();
            }
        });
        let mut s = Setup {
            printers: printers(),
            printer: 0,
            devmode: None,
            papers: Vec::new(),
            paper: 0,
            color_device: false,
            color: true,
            driver_copies: 1,
            orient: Orient::Auto,
            which: Which::All,
            from: cur + 1,
            to: cur + 1,
            copies: 1,
            current: cur,
            sizes,
            sheet: Sheet::a4(),
            shown: 0,
            images: HashMap::new(),
            asked: HashSet::new(),
            tex: None,
            want,
            got,
            error: None,
        };
        s.load_printer();
        s
    }

    fn printer_name(&self) -> Option<&str> {
        self.printers.get(self.printer).map(String::as_str)
    }

    /// Read the chosen printer's defaults and what it can do.
    fn load_printer(&mut self) {
        let Some(name) = self.printer_name().map(str::to_string) else { return };
        self.devmode = devmode(&name, None, false);
        self.papers = papers(&name, self.devmode.as_ref());
        self.color_device = capability(&name, DC_COLORDEVICE, None, self.devmode.as_ref()) == 1;
        self.driver_copies = capability(&name, DC_COPIES, None, self.devmode.as_ref()).max(1) as u32;
        self.orient = Orient::Auto;
        self.read_back(true);
        self.refresh();
    }

    /// Take over the paper, orientation, color and copies from the DEVMODE.
    fn read_back(&mut self, fresh: bool) {
        let Some(dm) = self.devmode.as_ref() else { return };
        let d = dm.get();
        let o = unsafe { d.Anonymous1.Anonymous1 };
        self.paper = if d.dmFields.0 & DM_PAPERSIZE.0 != 0 { o.dmPaperSize } else { 0 };
        if d.dmFields.0 & DM_ORIENTATION.0 != 0 && !fresh {
            if o.dmOrientation as u32 == DMORIENT_LANDSCAPE {
                self.orient = Orient::Landscape;
            } else if self.orient == Orient::Landscape {
                self.orient = Orient::Portrait;
            }
        }
        if self.color_device && d.dmFields.0 & DM_COLOR.0 != 0 {
            self.color = d.dmColor != DMCOLOR_MONOCHROME;
        }
        if !fresh && d.dmFields.0 & DM_COPIES.0 != 0 && o.dmCopies > 1 {
            self.copies = o.dmCopies as u32;
        }
    }

    /// Write the dialog's choices into the DEVMODE.
    fn apply(&mut self) {
        let (orient, paper, color_device, color) = (self.orient, self.paper, self.color_device, self.color);
        let copies = if self.copies <= self.driver_copies { self.copies } else { 1 };
        let Some(dm) = self.devmode.as_mut() else { return };
        let d = dm.get_mut();
        let mut fields = d.dmFields.0;
        unsafe {
            let o = &mut d.Anonymous1.Anonymous1;
            o.dmOrientation = if orient == Orient::Landscape { DMORIENT_LANDSCAPE } else { DMORIENT_PORTRAIT } as i16;
            fields |= DM_ORIENTATION.0;
            if paper != 0 {
                o.dmPaperSize = paper;
                fields = (fields | DM_PAPERSIZE.0) & !(DM_PAPERLENGTH.0 | DM_PAPERWIDTH.0);
            }
            o.dmCopies = copies as i16;
            fields |= DM_COPIES.0;
        }
        d.dmCollate = DMCOLLATE_TRUE;
        fields |= DM_COLLATE.0;
        if color_device {
            d.dmColor = if color { DMCOLOR_COLOR } else { DMCOLOR_MONOCHROME };
            fields |= DM_COLOR.0;
        }
        d.dmFields = DEVMODE_FIELD_FLAGS(fields);
    }

    /// Ask the printer for the sheet that the current choices give.
    fn refresh(&mut self) {
        self.apply();
        let Some(name) = self.printer_name() else { return };
        let hdc = unsafe { CreateICW(PCWSTR::null(), &HSTRING::from(name), PCWSTR::null(), self.devmode.as_ref().map(DevMode::ptr)) };
        if hdc.is_invalid() {
            self.sheet = Sheet::a4();
            return;
        }
        self.sheet = Sheet::of(hdc);
        unsafe {
            let _ = DeleteDC(hdc);
        }
    }

    /// The driver's own settings window.
    fn driver_settings(&mut self) {
        let Some(name) = self.printer_name().map(str::to_string) else { return };
        self.apply();
        if let Some(dm) = devmode(&name, self.devmode.as_ref(), true) {
            self.devmode = Some(dm);
            self.read_back(false);
            self.refresh();
        }
    }

    /// 0-based, inclusive.
    fn range(&self) -> (usize, usize) {
        let n = self.sizes.len().max(1);
        match self.which {
            Which::All => (0, n - 1),
            Which::Current => (self.current, self.current),
            Which::Range => {
                let (a, b) = (self.from.min(self.to).clamp(1, n), self.from.max(self.to).clamp(1, n));
                (a - 1, b - 1)
            }
        }
    }

    fn make_job(&mut self) -> Result<PrintJob, String> {
        self.apply();
        let name = self.printer_name().ok_or("No printer is set up on this computer.")?;
        let hdc = unsafe { CreateDCW(PCWSTR::null(), &HSTRING::from(name), PCWSTR::null(), self.devmode.as_ref().map(DevMode::ptr)) };
        if hdc.is_invalid() {
            return Err(format!("Cannot use the printer {name}."));
        }
        let (first, last) = self.range();
        let copies = if self.copies <= self.driver_copies { 1 } else { self.copies };
        Ok(PrintJob { dc: PrinterDc(hdc), first, last, auto_turn: self.orient == Orient::Auto, copies })
    }

    /// Draw the dialog. `Some(Some(job))` to print, `Some(None)` when cancelled,
    /// `None` while it stays open.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<Option<PrintJob>> {
        while let Ok((i, img)) = self.got.try_recv() {
            self.images.insert(i, img);
        }
        let mut answer = None;
        egui::Modal::new(egui::Id::unique("print_setup")).show(ctx, |ui| {
            ui.set_width(880.0);
            if self.printers.is_empty() {
                ui.heading("Print");
                ui.add_space(6.0);
                ui.label("No printer is set up on this computer.");
                ui.label("Add one in Windows Settings, Printers & scanners, then try again.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Look again").clicked() {
                        self.printers = printers();
                        self.load_printer();
                    }
                    if ui.button("Close").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        answer = Some(None);
                    }
                });
                return;
            }
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(300.0);
                    self.options(ui, &mut answer);
                });
                ui.add_space(14.0);
                ui.vertical(|ui| self.preview(ui));
            });
        });
        answer
    }

    fn options(&mut self, ui: &mut egui::Ui, answer: &mut Option<Option<PrintJob>>) {
        ui.heading("Print");
        ui.add_space(10.0);
        let mut changed = false;
        let n = self.sizes.len().max(1);

        ui.label("Printer");
        let before = self.printer;
        egui::ComboBox::from_id_salt("printer").width(292.0).selected_text(self.printers[self.printer].as_str()).show_ui(ui, |ui| {
            for (i, p) in self.printers.iter().enumerate() {
                ui.selectable_value(&mut self.printer, i, p.as_str());
            }
        });
        if self.printer != before {
            self.load_printer();
        }
        if ui.small_button("Printer settings...").on_hover_text("The printer's own settings: two-sided, paper tray, quality").clicked() {
            self.driver_settings();
        }
        ui.add_space(10.0);

        ui.label("Pages");
        let old = (self.which, self.from, self.to);
        ui.radio_value(&mut self.which, Which::All, format!("All ({n})"));
        ui.radio_value(&mut self.which, Which::Current, format!("Current page ({})", self.current + 1));
        ui.horizontal(|ui| {
            ui.radio_value(&mut self.which, Which::Range, "From");
            if ui.add(egui::DragValue::new(&mut self.from).range(1..=n)).changed() {
                self.which = Which::Range;
            }
            ui.label("to");
            if ui.add(egui::DragValue::new(&mut self.to).range(1..=n)).changed() {
                self.which = Which::Range;
            }
        });
        if old != (self.which, self.from, self.to) {
            self.shown = 0;
        }
        ui.add_space(10.0);

        egui::Grid::new("print_grid").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
            ui.label("Copies");
            ui.add(egui::DragValue::new(&mut self.copies).range(1..=999));
            ui.end_row();

            if !self.papers.is_empty() {
                ui.label("Paper");
                let shown = self.papers.iter().find(|(id, _)| *id == self.paper).map_or("Printer's choice", |(_, n)| n.as_str());
                let before = self.paper;
                egui::ComboBox::from_id_salt("paper").width(210.0).selected_text(shown).show_ui(ui, |ui| {
                    for (id, name) in &self.papers {
                        ui.selectable_value(&mut self.paper, *id, name.as_str());
                    }
                });
                changed |= self.paper != before;
                ui.end_row();
            }

            ui.label("Orientation");
            let before = self.orient;
            let label = |o: Orient| match o {
                Orient::Auto => "Automatic",
                Orient::Portrait => "Portrait",
                Orient::Landscape => "Landscape",
            };
            egui::ComboBox::from_id_salt("orient").width(210.0).selected_text(label(self.orient)).show_ui(ui, |ui| {
                for o in [Orient::Auto, Orient::Portrait, Orient::Landscape] {
                    ui.selectable_value(&mut self.orient, o, label(o)).on_hover_text(match o {
                        Orient::Auto => "Landscape pages are turned to fill the sheet",
                        _ => "Pages are printed upright, shrunk to fit the sheet",
                    });
                }
            });
            changed |= self.orient != before;
            ui.end_row();

            if self.color_device {
                ui.label("Color");
                let before = self.color;
                egui::ComboBox::from_id_salt("color").width(210.0).selected_text(if self.color { "Color" } else { "Black and white" }).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.color, true, "Color");
                    ui.selectable_value(&mut self.color, false, "Black and white");
                });
                changed |= self.color != before;
                ui.end_row();
            }
        });
        if changed {
            self.refresh();
        }

        if let Some(e) = &self.error {
            ui.add_space(8.0);
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        ui.add_space(18.0);
        ui.horizontal(|ui| {
            let go = ui.add_sized([110.0, 30.0], egui::Button::new("Print")).clicked()
                || (ui.input(|i| i.key_pressed(egui::Key::Enter)) && !ui.ctx().egui_wants_keyboard_input());
            if go {
                match self.make_job() {
                    Ok(job) => *answer = Some(Some(job)),
                    Err(e) => self.error = Some(e),
                }
            }
            if ui.add_sized([90.0, 30.0], egui::Button::new("Cancel")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                *answer = Some(None);
            }
        });
    }

    fn preview(&mut self, ui: &mut egui::Ui) {
        let (first, last) = self.range();
        let count = last - first + 1;
        self.shown = self.shown.min(count - 1);
        let idx = first + self.shown;
        let s = self.sheet;
        let (pw, ph) = self.sizes.get(idx).copied().unwrap_or((595.0, 842.0));
        let p = place(pw, ph, &s, self.orient == Orient::Auto);

        let (area, resp) = ui.allocate_exact_size(vec2(540.0, 520.0), Sense::click());
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 6.0, ui.visuals().extreme_bg_color);

        // The sheet, as large as fits.
        let (sw, sh) = (s.phys_w / s.dpi_x, s.phys_h / s.dpi_y);
        let k = ((area.width() - 40.0) / sw).min((area.height() - 40.0) / sh);
        let sheet = Rect::from_center_size(area.center(), vec2(sw * k, sh * k));
        painter.rect_filled(sheet.translate(vec2(0.0, 3.0)).expand(1.0), 1.0, Color32::from_black_alpha(70));
        painter.rect_filled(sheet, 0.0, Color32::WHITE);
        // Device pixels to screen.
        let (kx, ky) = (sheet.width() / s.phys_w, sheet.height() / s.phys_h);
        if s.off_x > 0.0 || s.off_y > 0.0 {
            let printable = Rect::from_min_size(sheet.min + vec2(s.off_x * kx, s.off_y * ky), vec2(s.res_x * kx, s.res_y * ky));
            painter.rect_stroke(printable, 0.0, Stroke::new(1.0, Color32::from_gray(225)), StrokeKind::Inside);
        }
        let page = Rect::from_min_size(
            sheet.min + vec2((s.off_x + p.dx) * kx, (s.off_y + p.dy) * ky),
            vec2(p.dw * kx, p.dh * ky),
        );

        // The page image, in color or gray as it will print.
        let gray = self.color_device && !self.color;
        if self.tex.as_ref().is_none_or(|(key, _)| *key != (idx, gray)) {
            if let Some((w, h, rgba)) = self.images.get(&idx) {
                let mut px = rgba.clone();
                if gray {
                    for c in px.chunks_exact_mut(4) {
                        let y = (0.299 * c[0] as f32 + 0.587 * c[1] as f32 + 0.114 * c[2] as f32) as u8;
                        c[..3].fill(y);
                    }
                }
                let img = ColorImage::from_rgba_unmultiplied([*w, *h], &px);
                self.tex = Some(((idx, gray), ui.ctx().load_texture("print_preview", img, TextureOptions::LINEAR)));
            } else {
                if self.asked.insert(idx) {
                    let _ = self.want.send(idx);
                }
            }
        }
        // The next sheet is rendered ahead so paging through is quick.
        if idx < last && self.images.contains_key(&idx) && self.asked.insert(idx + 1) {
            let _ = self.want.send(idx + 1);
        }
        match self.tex.as_ref().filter(|(key, _)| *key == (idx, gray)) {
            Some((_, tex)) => {
                let mut mesh = Mesh::with_texture(tex.id());
                // Corners top-left, top-right, bottom-right, bottom-left, and which image corner each shows.
                let uv = if p.turn {
                    [pos2(0.0, 1.0), pos2(0.0, 0.0), pos2(1.0, 0.0), pos2(1.0, 1.0)]
                } else {
                    [pos2(0.0, 0.0), pos2(1.0, 0.0), pos2(1.0, 1.0), pos2(0.0, 1.0)]
                };
                for (corner, uv) in [page.left_top(), page.right_top(), page.right_bottom(), page.left_bottom()].into_iter().zip(uv) {
                    mesh.vertices.push(egui::epaint::Vertex { pos: corner, uv, color: Color32::WHITE });
                }
                mesh.indices.extend([0, 1, 2, 0, 2, 3]);
                painter.add(mesh);
            }
            None => {
                painter.rect_stroke(page, 0.0, Stroke::new(1.0, Color32::from_gray(210)), StrokeKind::Inside);
                egui::Spinner::new().paint_at(ui, Rect::from_center_size(page.center(), vec2(24.0, 24.0)));
            }
        }
        // Scroll over the preview to page through.
        if resp.hovered() {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy < -0.5 && self.shown + 1 < count {
                self.shown += 1;
            } else if dy > 0.5 && self.shown > 0 {
                self.shown -= 1;
            }
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.add_enabled(self.shown > 0, egui::Button::new("<")).clicked() {
                self.shown -= 1;
            }
            ui.label(format!("Sheet {} of {count}", self.shown + 1));
            if ui.add_enabled(self.shown + 1 < count, egui::Button::new(">")).clicked() {
                self.shown += 1;
            }
            ui.add_space(10.0);
            let (mw, mh) = s.mm();
            let paper = match self.papers.iter().find(|(id, _)| *id == self.paper) {
                Some((_, name)) => name.clone(),
                None => format!("{mw:.0} x {mh:.0} mm"),
            };
            let mut info = format!("Page {} on {paper}", idx + 1);
            if p.fit < 0.995 {
                info.push_str(&format!(" - shrunk to {:.0}%", p.fit * 100.0));
            }
            if self.copies > 1 {
                info.push_str(&format!(" - {} copies", self.copies));
            }
            ui.label(egui::RichText::new(info).weak());
        });
    }
}

// ---------------------------------------------------------------------------
// Printing
// ---------------------------------------------------------------------------

/// Render and spool the pages on a background thread.
pub fn start(
    job: PrintJob,
    bytes: Arc<Vec<u8>>,
    password: String,
    title: String,
    tx: Sender<Progress>,
    wake: impl Fn() + Send + 'static,
) {
    let _ = std::thread::Builder::new().name("print".into()).stack_size(16 * 1024 * 1024).spawn(move || {
        let job = job;
        let hdc = job.dc.0;
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(hdc, job.first, job.last, job.auto_turn, job.copies, bytes, &password, &title, None, &tx, &wake)
        }));
        let msg = match res {
            Ok(Ok(())) => Progress::Done,
            Ok(Err(e)) => Progress::Failed(e),
            Err(_) => Progress::Failed("Printing stopped: a page could not be rendered.".into()),
        };
        if !matches!(msg, Progress::Done) {
            unsafe { AbortDoc(hdc) };
        }
        unsafe {
            let _ = DeleteDC(hdc);
        }
        let _ = tx.send(msg);
        wake();
    });
}

#[allow(clippy::too_many_arguments)]
fn run(
    hdc: HDC,
    first: usize,
    last: usize,
    auto_turn: bool,
    copies: u32,
    bytes: Arc<Vec<u8>>,
    password: &str,
    title: &str,
    output: Option<&str>,
    tx: &Sender<Progress>,
    wake: &dyn Fn(),
) -> Result<(), String> {
    let pdf = render::open_pdf(bytes, password).map_err(|_| "Cannot read the PDF for printing.".to_string())?;
    let pages = pdf.pages();
    let settings = render::interpreter_settings();
    let mut cache = RenderCache::new();

    let name: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let out_name: Option<Vec<u16>> = output.map(|o| o.encode_utf16().chain(std::iter::once(0)).collect());
    let di = DOCINFOW {
        cbSize: std::mem::size_of::<DOCINFOW>() as i32,
        lpszDocName: PCWSTR(name.as_ptr()),
        lpszOutput: out_name.as_ref().map_or(PCWSTR::null(), |o| PCWSTR(o.as_ptr())),
        ..Default::default()
    };
    if unsafe { StartDocW(hdc, &di) } <= 0 {
        return Err("The printer did not accept the job.".into());
    }

    let sheet = Sheet::of(hdc);
    unsafe { SetStretchBltMode(hdc, HALFTONE) };

    let per_copy = last - first + 1;
    let total = per_copy * copies.max(1) as usize;
    let order = (0..copies.max(1)).flat_map(|_| first..=last);
    for (n, idx) in order.enumerate() {
        let Some(page) = pages.get(idx) else { break };
        let _ = tx.send(Progress::Page(n + 1, total));
        wake();

        let (pw, ph) = page.render_dimensions();
        let p = place(pw, ph, &sheet, auto_turn);

        // 300 dpi is plenty for a bitmap and keeps memory in check.
        let dpi = sheet.dpi_x.min(sheet.dpi_y).min(300.0);
        let width_px = (pw / 72.0 * dpi * p.fit).round().max(1.0) as u32;
        let (w, h, rgba) = render::render_page(page, &cache, &settings, width_px, 14000);
        let (bw, bh, bgr) = to_bgr24(&rgba, w, h, p.turn);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: bw as i32,
                biHeight: bh as i32, // bottom-up
                biPlanes: 1,
                biBitCount: 24,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        unsafe {
            if StartPage(hdc) <= 0 {
                return Err("The printer stopped the job.".into());
            }
            StretchDIBits(
                hdc,
                p.dx as i32,
                p.dy as i32,
                p.dw as i32,
                p.dh as i32,
                0,
                0,
                bw as i32,
                bh as i32,
                Some(bgr.as_ptr().cast()),
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
            if EndPage(hdc) <= 0 {
                return Err("The printer stopped the job.".into());
            }
        }
        if (n + 1) % 16 == 0 {
            cache = RenderCache::new();
        }
    }
    if unsafe { EndDoc(hdc) } <= 0 {
        return Err("The printer did not finish the job.".into());
    }
    Ok(())
}

/// Print a whole file without showing a dialog: to the default printer, or to
/// the named one. `output` sends the result to a file (for printers that
/// write files, such as "Microsoft Print to PDF"). Used by the Explorer
/// "Print" command and by `ProtoPDF --print`.
pub fn print_file(path: &std::path::Path, printer: Option<&str>, output: Option<&str>) -> Result<(), String> {
    let bytes = Arc::new(std::fs::read(path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?);
    let pages = render::open_pdf(bytes.clone(), "").map_err(|_| "Cannot read the PDF.".to_string())?.pages().len();
    if pages == 0 {
        return Err("The PDF has no pages.".into());
    }
    let printer = match printer {
        Some(p) => p.to_string(),
        None => default_printer().ok_or("No default printer is set.")?,
    };
    let hdc = unsafe { CreateDCW(PCWSTR::null(), &HSTRING::from(printer.as_str()), PCWSTR::null(), None) };
    if hdc.is_invalid() {
        return Err(format!("Cannot use the printer {printer}."));
    }
    let title = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let (tx, _rx) = std::sync::mpsc::channel();
    let res = run(hdc, 0, pages - 1, true, 1, bytes, "", &title, output, &tx, &|| {});
    unsafe {
        if res.is_err() {
            AbortDoc(hdc);
        }
        let _ = DeleteDC(hdc);
    }
    res
}

/// Convert opaque RGBA to a bottom-up 24-bit BGR bitmap with rows padded to
/// four bytes, optionally turned 90 degrees clockwise.
fn to_bgr24(rgba: &[u8], w: usize, h: usize, turn: bool) -> (usize, usize, Vec<u8>) {
    let (bw, bh) = if turn { (h, w) } else { (w, h) };
    let stride = (bw * 3 + 3) & !3;
    let mut out = vec![255u8; stride * bh];
    for y in 0..bh {
        let row = &mut out[(bh - 1 - y) * stride..(bh - 1 - y) * stride + bw * 3];
        for x in 0..bw {
            // Destination (x, y) of a clockwise turn comes from source (y, h - 1 - x).
            let (sx, sy) = if turn { (y, h - 1 - x) } else { (x, y) };
            let s = (sy * w + sx) * 4;
            row[x * 3] = rgba[s + 2];
            row[x * 3 + 1] = rgba[s + 1];
            row[x * 3 + 2] = rgba[s];
        }
    }
    (bw, bh, out)
}
