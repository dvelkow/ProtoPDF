#![windows_subsystem = "windows"]

mod app;
mod edit;
mod info;
mod print;
mod register;
mod render;
mod sign;
mod text;

use std::path::PathBuf;

pub fn fonts_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into())).join("Fonts")
}

static ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

fn main() {
    // File names are taken as Windows gives them; they are not always valid Unicode.
    let raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let args: Vec<String> = raw.iter().map(|a| a.to_string_lossy().into_owned()).collect();

    match args.first().map(String::as_str) {
        // ProtoPDF --register [--quiet]: add the program to Windows' list of PDF apps.
        Some("--register") => {
            let exe = std::env::current_exe().expect("exe path");
            let code = match register::register(&exe) {
                Ok(()) => {
                    if !args.iter().any(|a| a == "--quiet") {
                        register::open_settings();
                    }
                    0
                }
                Err(_) => 1,
            };
            std::process::exit(code);
        }
        // ProtoPDF --unregister: remove the program from Windows' list of PDF apps.
        Some("--unregister") => {
            register::unregister();
            std::process::exit(0);
        }
        // ProtoPDF --print file.pdf [--printer "name"] [--output file]: print without opening a window.
        Some("--print") => {
            let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
            let code = match args.get(1) {
                Some(file) => match print::print_file(std::path::Path::new(file), opt("--printer").as_deref(), opt("--output").as_deref()) {
                    Ok(()) => 0,
                    Err(_) => 1,
                },
                None => 1,
            };
            std::process::exit(code);
        }
        // ProtoPDF --verify file.pdf: check the digital signatures and print the result.
        // Exit code 0 = all valid and trusted, 2 = unchanged but signer not confirmed,
        // 3 = a signature is not valid, 4 = the file is not signed.
        Some("--verify") => {
            let code = match args.get(1).map(std::fs::read) {
                Some(Ok(bytes)) => {
                    let sigs = sign::verify_all(&bytes);
                    for (i, s) in sigs.iter().enumerate() {
                        println!("Signature {}: {}", i + 1, if s.is_timestamp { "document timestamp" } else { s.signer.as_str() });
                        if !s.org.is_empty() {
                            println!("  Organisation: {}", s.org);
                        }
                        println!("  Issued by: {}", s.issuer);
                        println!("  Signed at: {} (signer's clock)", s.signed_at);
                        if let Some(t) = &s.timestamp {
                            println!("  Timestamp: {t}");
                        }
                        println!("  Document: {}", s.intact_msg);
                        println!("  Signer: {}", s.trust_msg);
                    }
                    if sigs.is_empty() {
                        println!("This PDF has no digital signature.");
                        4
                    } else {
                        let (level, text) = sign::summary(&sigs);
                        println!("{text}");
                        match level {
                            sign::Level::Good => 0,
                            sign::Level::Warn => 2,
                            sign::Level::Bad => 3,
                        }
                    }
                }
                _ => 1,
            };
            std::process::exit(code);
        }
        // ProtoPDF --sign in.pdf out.pdf --thumbprint HEX [--page N] [--rect x0,y0,x1,y1] [--reason text] [--location text]
        // signs with the certificate that has this SHA-1 thumbprint in the Windows store.
        Some("--sign") => {
            let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
            let run = || -> Result<(), String> {
                let (input, output) = (args.get(1).ok_or("input")?, args.get(2).ok_or("output")?);
                let cert = sign::find_certificate(&opt("--thumbprint").ok_or("--thumbprint is required")?)
                    .ok_or("No signing certificate with this thumbprint")?;
                let rect = opt("--rect").and_then(|r| {
                    let v: Vec<f32> = r.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                    (v.len() == 4).then(|| [v[0], v[1], v[2], v[3]])
                });
                let options = sign::SignOptions {
                    page: opt("--page").and_then(|p| p.parse::<usize>().ok()).unwrap_or(1).saturating_sub(1),
                    rect,
                    reason: opt("--reason").unwrap_or_default(),
                    location: opt("--location").unwrap_or_default(),
                };
                let bytes = std::fs::read(input).map_err(|e| e.to_string())?;
                let signed = sign::sign_pdf(&bytes, &cert, &options)?;
                std::fs::write(output, signed).map_err(|e| e.to_string())
            };
            let code = match run() {
                Ok(()) => 0,
                Err(e) => {
                    println!("{e}");
                    1
                }
            };
            std::process::exit(code);
        }
        // ProtoPDF --render in.pdf page out.png [width]: render one page to a PNG.
        Some("--render") => {
            let code = match cli_render(&args[1..]) {
                Ok(()) => 0,
                Err(_) => 1,
            };
            std::process::exit(code);
        }
        _ => {}
    }

    let file = raw.first().filter(|a| !a.to_string_lossy().starts_with("--")).map(PathBuf::from);
    let shot = args
        .iter()
        .position(|a| a == "--screenshot")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("ProtoPDF")
        .with_app_id("ProtoPDF")
        .with_inner_size([1100.0, 860.0])
        .with_min_inner_size([520.0, 380.0])
        .with_drag_and_drop(true);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let _ = eframe::run_native(
        "ProtoPDF",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, file, shot)))),
    );
}

fn cli_render(args: &[String]) -> Result<(), String> {
    use hayro::vello_cpu::color::palette::css::WHITE;
    let [input, page, output, rest @ ..] = args else { return Err("usage".into()) };
    let page: usize = page.parse().map_err(|_| "page")?;
    let width: f32 = rest.first().and_then(|w| w.parse().ok()).unwrap_or(1000.0);
    let bytes = std::sync::Arc::new(std::fs::read(input).map_err(|e| e.to_string())?);
    let pdf = render::open_pdf(bytes, "").map_err(|_| "open")?;
    let page = pdf.pages().get(page.saturating_sub(1)).ok_or("page")?;
    let scale = width / page.render_dimensions().0.max(1.0);
    let pix = hayro::render(
        page,
        &hayro::RenderCache::new(),
        &render::interpreter_settings(),
        &hayro::RenderSettings::default(),
        &hayro::PixmapSettings { x_scale: scale, y_scale: scale, bg_color: WHITE },
    );
    std::fs::write(output, pix.into_png().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}
