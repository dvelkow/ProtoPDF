//! Printing on Linux through CUPS, the print system every Linux desktop uses.
//!
//! The PDF itself is handed to CUPS with `lp`. CUPS prints PDF natively, at
//! the printer's full resolution, so text stays sharp (on Windows pages go
//! out as bitmaps). CUPS also turns landscape pages to fill the sheet. The
//! dialog that picks the printer, the pages and the number of copies is drawn
//! by ProtoPDF itself, so it works the same on every desktop.

use eframe::egui;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::Sender;

pub enum Progress {
    #[allow(dead_code)] // Only the Windows printer reports page by page.
    Page(usize, usize),
    Done,
    Failed(String),
}

pub struct PrintJob {
    printer: String,
    /// 0-based, inclusive. `None` prints every page.
    pages: Option<(usize, usize)>,
    copies: u32,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Which {
    All,
    Current,
    Range,
}

/// The print dialog's state.
pub struct Setup {
    printers: Vec<String>,
    printer: usize,
    which: Which,
    from: usize,
    to: usize,
    copies: u32,
    page_count: usize,
    current: usize,
}

/// Run a CUPS command with English output, so its answers can be read.
fn cups(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).env("LC_ALL", "C").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The installed printers, the default one first.
fn printers() -> Vec<String> {
    let mut list: Vec<String> = cups("lpstat", &["-e"]).unwrap_or_default().lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
    if let Some(default) = default_printer() {
        if let Some(i) = list.iter().position(|p| *p == default) {
            let d = list.remove(i);
            list.insert(0, d);
        }
    }
    list
}

fn default_printer() -> Option<String> {
    let out = cups("lpstat", &["-d"])?;
    let name = out.lines().find_map(|l| l.split_once("system default destination:").map(|(_, n)| n.trim().to_string()))?;
    (!name.is_empty()).then_some(name)
}

impl Setup {
    pub fn new(page_count: usize, current_page: usize) -> Self {
        let n = page_count.max(1);
        let cur = current_page.min(n - 1);
        Setup { printers: printers(), printer: 0, which: Which::All, from: cur + 1, to: cur + 1, copies: 1, page_count: n, current: cur }
    }

    /// Draw the dialog. `Some(Some(job))` to print, `Some(None)` when cancelled,
    /// `None` while it stays open.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<Option<PrintJob>> {
        let mut answer = None;
        egui::Modal::new(egui::Id::unique("print_setup")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.heading("Print");
            ui.add_space(6.0);
            if self.printers.is_empty() {
                ui.label("No printer is set up on this computer.");
                ui.label("Add one in Settings, Printers, then try again.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Look again").clicked() {
                        self.printers = printers();
                    }
                    if ui.button("Close").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        answer = Some(None);
                    }
                });
                return;
            }
            egui::Grid::new("print_grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                ui.label("Printer");
                egui::ComboBox::from_id_salt("printer").width(230.0).selected_text(self.printers[self.printer].as_str()).show_ui(ui, |ui| {
                    for (i, p) in self.printers.iter().enumerate() {
                        ui.selectable_value(&mut self.printer, i, p.as_str());
                    }
                });
                ui.end_row();

                ui.label("Pages");
                ui.vertical(|ui| {
                    ui.radio_value(&mut self.which, Which::All, format!("All ({})", self.page_count));
                    ui.radio_value(&mut self.which, Which::Current, format!("Current page ({})", self.current + 1));
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut self.which, Which::Range, "From");
                        let n = self.page_count;
                        if ui.add(egui::DragValue::new(&mut self.from).range(1..=n)).changed() {
                            self.which = Which::Range;
                        }
                        ui.label("to");
                        if ui.add(egui::DragValue::new(&mut self.to).range(1..=n)).changed() {
                            self.which = Which::Range;
                        }
                    });
                });
                ui.end_row();

                ui.label("Copies");
                ui.add(egui::DragValue::new(&mut self.copies).range(1..=999));
                ui.end_row();
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Print").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let pages = match self.which {
                        Which::All => None,
                        Which::Current => Some((self.current, self.current)),
                        Which::Range => {
                            let (a, b) = (self.from.min(self.to).max(1), self.from.max(self.to).min(self.page_count));
                            Some((a - 1, b - 1))
                        }
                    };
                    answer = Some(Some(PrintJob { printer: self.printers[self.printer].clone(), pages, copies: self.copies.max(1) }));
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    answer = Some(None);
                }
            });
        });
        answer
    }
}

/// The bytes to give CUPS. A file opened with a password is written out
/// without it, because CUPS cannot open it otherwise.
fn printable(bytes: &[u8], password: &str) -> Result<Vec<u8>, String> {
    if password.is_empty() {
        return Ok(bytes.to_vec());
    }
    let mut doc = crate::edit::load(bytes, password).map_err(|e| format!("Cannot prepare the PDF for printing.\n{e}"))?;
    let mut out = Vec::new();
    doc.save_to(&mut out).map_err(|e| format!("Cannot prepare the PDF for printing.\n{e}"))?;
    Ok(out)
}

/// Hand a PDF to CUPS.
fn send(printer: &str, title: &str, pages: Option<(usize, usize)>, copies: u32, pdf: &[u8]) -> Result<(), String> {
    let mut args: Vec<String> = vec!["-d".into(), printer.into(), "-t".into(), title.into(), "-n".into(), copies.max(1).to_string()];
    if let Some((a, b)) = pages {
        args.push("-P".into());
        args.push(format!("{}-{}", a + 1, b + 1));
    }
    // Pages larger than the printable area are shrunk to fit, smaller ones print at actual size.
    args.extend(["-o".into(), "print-scaling=auto-fit".into()]);
    if copies > 1 {
        args.extend(["-o".into(), "collate=true".into()]);
    }
    let mut child = Command::new("lp")
        .args(&args)
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Cannot start the print system (lp): {e}\nInstall it with: sudo apt install cups-client"))?;
    let write = child.stdin.take().map(|mut s| s.write_all(pdf));
    let out = child.wait_with_output().map_err(|e| format!("The print system stopped: {e}"))?;
    if out.status.success() && !matches!(write, Some(Err(_))) {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("lp: ").to_string();
        Err(if msg.is_empty() { format!("The printer {printer} did not accept the job.") } else { format!("The printer {printer} did not accept the job:\n{msg}") })
    }
}

/// Send the job on a background thread.
pub fn start(
    job: PrintJob,
    bytes: Arc<Vec<u8>>,
    password: String,
    title: String,
    tx: Sender<Progress>,
    wake: impl Fn() + Send + 'static,
) {
    let _ = std::thread::Builder::new().name("print".into()).spawn(move || {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let pdf = printable(&bytes, &password)?;
            send(&job.printer, &title, job.pages, job.copies, &pdf)
        }));
        let msg = match res {
            Ok(Ok(())) => Progress::Done,
            Ok(Err(e)) => Progress::Failed(e),
            Err(_) => Progress::Failed("Printing stopped: the PDF could not be prepared.".into()),
        };
        let _ = tx.send(msg);
        wake();
    });
}

/// Print a whole file without showing a dialog: to the default printer, or to
/// the named one. With `output` nothing is printed: the PDF that would go to
/// the printer is written to that file instead. Used by `ProtoPDF --print`.
pub fn print_file(path: &std::path::Path, printer: Option<&str>, output: Option<&str>) -> Result<(), String> {
    let bytes = Arc::new(std::fs::read(path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?);
    let pages = crate::render::open_pdf(bytes.clone(), "").map_err(|_| "Cannot read the PDF.".to_string())?.pages().len();
    if pages == 0 {
        return Err("The PDF has no pages.".into());
    }
    if let Some(out) = output {
        return std::fs::write(out, printable(&bytes, "")?).map_err(|e| format!("Cannot write {out}: {e}"));
    }
    let printer = match printer {
        Some(p) => p.to_string(),
        None => default_printer().ok_or("No default printer is set.")?,
    };
    let title = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    send(&printer, &title, None, 1, &bytes)
}
