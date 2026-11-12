//! Printing through the standard Windows print dialog. Pages are rendered to
//! bitmaps and sent to the printer with GDI, so any installed printer works
//! (including "Microsoft Print to PDF").

use crate::render;
use hayro::RenderCache;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use windows::Win32::Foundation::GlobalFree;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDCW, DIB_RGB_COLORS, DeleteDC, GetDeviceCaps, HALFTONE, HDC,
    HORZRES, LOGPIXELSX, LOGPIXELSY, SRCCOPY, SetStretchBltMode, StretchDIBits, VERTRES,
};
use windows::Win32::Graphics::Printing::GetDefaultPrinterW;
use windows::Win32::Storage::Xps::{AbortDoc, DOCINFOW, EndDoc, EndPage, StartDocW, StartPage};
use windows::Win32::UI::Controls::Dialogs::{
    PD_NOSELECTION, PD_PAGENUMS, PD_RETURNDC, PD_USEDEVMODECOPIESANDCOLLATE, PRINTDLGW, PrintDlgW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows::core::{HSTRING, PCWSTR, PWSTR};

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
}

/// Show the print dialog. Returns `None` if the user cancelled.
pub fn choose_printer(page_count: usize, current_page: usize) -> Option<PrintJob> {
    let max = page_count.clamp(1, u16::MAX as usize) as u16;
    let cur = (current_page + 1).clamp(1, max as usize) as u16;
    let mut pd = PRINTDLGW {
        lStructSize: std::mem::size_of::<PRINTDLGW>() as u32,
        hwndOwner: unsafe { GetActiveWindow() },
        Flags: PD_RETURNDC | PD_NOSELECTION | PD_USEDEVMODECOPIESANDCOLLATE,
        nFromPage: cur,
        nToPage: cur,
        nMinPage: 1,
        nMaxPage: max,
        nCopies: 1,
        ..Default::default()
    };
    let ok = unsafe { PrintDlgW(&mut pd) }.as_bool();
    unsafe {
        if !pd.hDevMode.is_invalid() {
            let _ = GlobalFree(Some(pd.hDevMode));
        }
        if !pd.hDevNames.is_invalid() {
            let _ = GlobalFree(Some(pd.hDevNames));
        }
    }
    if !ok || pd.hDC.is_invalid() {
        return None;
    }
    let (first, last) = if pd.Flags.contains(PD_PAGENUMS) {
        let a = pd.nFromPage.clamp(1, max) as usize;
        let b = pd.nToPage.clamp(1, max) as usize;
        (a.min(b) - 1, a.max(b) - 1)
    } else {
        (0, page_count.saturating_sub(1))
    };
    Some(PrintJob { dc: PrinterDc(pd.hDC), first, last })
}

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
            run(hdc, job.first, job.last, bytes, &password, &title, None, &tx, &wake)
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

    let (res_x, res_y, dpi_x, dpi_y) = unsafe {
        (
            GetDeviceCaps(Some(hdc), HORZRES).max(1) as f32,
            GetDeviceCaps(Some(hdc), VERTRES).max(1) as f32,
            GetDeviceCaps(Some(hdc), LOGPIXELSX).max(72) as f32,
            GetDeviceCaps(Some(hdc), LOGPIXELSY).max(72) as f32,
        )
    };
    unsafe { SetStretchBltMode(hdc, HALFTONE) };

    let total = last - first + 1;
    for (n, idx) in (first..=last).enumerate() {
        let Some(page) = pages.get(idx) else { break };
        let _ = tx.send(Progress::Page(n + 1, total));
        wake();

        let (pw, ph) = page.render_dimensions();
        // A landscape page on portrait paper (or the reverse) is turned to fill the sheet.
        let turn = (pw > ph) != (res_x / dpi_x > res_y / dpi_y);
        let (sw, sh) = if turn { (ph, pw) } else { (pw, ph) };
        // Sheet size of the page in device pixels at actual size, shrunk to the printable area.
        let (full_w, full_h) = (sw / 72.0 * dpi_x, sh / 72.0 * dpi_y);
        let fit = (res_x / full_w).min(res_y / full_h).min(1.0);
        let (dw, dh) = ((full_w * fit).floor().max(1.0), (full_h * fit).floor().max(1.0));
        let (dx, dy) = (((res_x - dw) / 2.0).floor(), ((res_y - dh) / 2.0).floor());

        // 300 dpi is plenty for a bitmap and keeps memory in check.
        let dpi = dpi_x.min(dpi_y).min(300.0);
        let width_px = (pw / 72.0 * dpi * fit).round().max(1.0) as u32;
        let (w, h, rgba) = render::render_page(page, &cache, &settings, width_px, 14000);
        let (bw, bh, bgr) = to_bgr24(&rgba, w, h, turn);

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
                dx as i32,
                dy as i32,
                dw as i32,
                dh as i32,
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
        None => {
            let mut buf = [0u16; 512];
            let mut len = buf.len() as u32;
            if !unsafe { GetDefaultPrinterW(Some(PWSTR(buf.as_mut_ptr())), &mut len) }.as_bool() {
                return Err("No default printer is set.".into());
            }
            String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)])
        }
    };
    let hdc = unsafe { CreateDCW(PCWSTR::null(), &HSTRING::from(printer.as_str()), PCWSTR::null(), None) };
    if hdc.is_invalid() {
        return Err(format!("Cannot use the printer {printer}."));
    }
    let title = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let (tx, _rx) = std::sync::mpsc::channel();
    let res = run(hdc, 0, pages - 1, bytes, "", &title, output, &tx, &|| {});
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
