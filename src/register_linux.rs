//! Registers ProtoPDF with the Linux desktop as a program that opens PDF files.
//!
//! This follows the freedesktop.org standards that GNOME, KDE, Xfce, Cinnamon
//! and the others share: a desktop entry in `~/.local/share/applications`
//! saying the program handles `application/pdf`, and the default chosen in
//! `~/.config/mimeapps.list`. Everything is per user, no root needed. Unlike
//! Windows, Linux lets a program make itself the default, so `make_default`
//! does it directly.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const APP: &str = "ProtoPDF";
pub const DESKTOP_ID: &str = "protopdf.desktop";
const MIMES: [&str; 2] = ["application/pdf", "application/x-pdf"];

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(fallback))
}

fn data_home() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share")
}

fn config_home() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
}

fn applications_dir() -> PathBuf {
    data_home().join("applications")
}

pub fn desktop_file() -> PathBuf {
    applications_dir().join(DESKTOP_ID)
}

fn icon_file() -> PathBuf {
    data_home().join("icons/hicolor/128x128/apps/protopdf.png")
}

/// Write a file by writing a temporary one next to it and renaming it into
/// place, so a reader never sees half a file.
fn write_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("bad path")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".{}.tmp{}", path.file_name().and_then(|n| n.to_str()).unwrap_or("file"), std::process::id()));
    std::fs::write(&tmp, data).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// One argument of an `Exec=` line, quoted as the desktop entry standard asks.
/// Quotes are only added when needed: older xdg-utils (Ubuntu 22.04) cannot
/// find a program whose path is quoted, and then ignore the default.
fn exec_arg(s: &str) -> String {
    const RESERVED: &[char] = &[' ', '\t', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(', ')', '`', '='];
    if !s.is_empty() && !s.contains(RESERVED) {
        return s.replace('%', "%%");
    }
    let mut q = String::from("\"");
    for c in s.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                q.push('\\');
                q.push(c);
            }
            '%' => q.push_str("%%"),
            _ => q.push(c),
        }
    }
    q.push('"');
    // The value as a whole is a desktop entry string, in which backslashes are escaped once more.
    q.replace('\\', "\\\\")
}

fn run_quiet(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Install the icon and the desktop entry, so ProtoPDF shows up in the
/// application list and in "Open With" for PDF files.
pub fn register(exe: &Path) -> Result<(), String> {
    let exe = exe.to_str().ok_or("The program's path is not valid text.")?;
    if exe.contains(['\n', '\r']) {
        return Err("The program's path contains a line break.".into());
    }
    let icon = icon_file();
    write_atomic(&icon, crate::ICON_PNG)?;
    let icon = icon.to_str().ok_or("The icon path is not valid text.")?.replace('\\', "\\\\");

    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name={APP}\n\
         GenericName=PDF Reader\n\
         Comment=Read, search, edit, sign and print PDF files\n\
         Exec={exec} %f\n\
         TryExec={try_exec}\n\
         Icon={icon}\n\
         Terminal=false\n\
         StartupNotify=true\n\
         StartupWMClass={APP}\n\
         Categories=Office;Viewer;\n\
         Keywords=PDF;Reader;Viewer;Document;Sign;Print;\n\
         MimeType={mimes};\n",
        exec = exec_arg(exe),
        try_exec = exe.replace('\\', "\\\\"),
        mimes = MIMES.join(";"),
    );
    write_atomic(&desktop_file(), entry.as_bytes())?;

    // Refresh the cache of which programs open which file types. Desktops that
    // do not use the cache read the entry directly, so a failure here is harmless.
    let apps = applications_dir();
    let _ = run_quiet("update-desktop-database", &[&apps.to_string_lossy()]);
    forget_default_cache();
    Ok(())
}

/// Make ProtoPDF the program that opens PDF files. Returns what to tell the user.
pub fn make_default() -> Result<String, String> {
    if !desktop_file().exists() {
        return Err("ProtoPDF is not registered yet.".into());
    }
    let mut ok = true;
    for mime in MIMES {
        // xdg-mime works on every desktop; gio is the GNOME tool, used if xdg-utils is missing.
        ok &= run_quiet("xdg-mime", &["default", DESKTOP_ID, mime]) || run_quiet("gio", &["mime", mime, DESKTOP_ID]);
    }
    forget_default_cache();
    if ok && is_default() {
        Ok("ProtoPDF now opens PDF files".into())
    } else {
        Err("The desktop did not accept the change (xdg-mime failed). Right click a PDF, choose Open With, pick ProtoPDF and set it as default.".into())
    }
}

/// Remove everything `register` and `make_default` wrote.
pub fn unregister() {
    let _ = std::fs::remove_file(desktop_file());
    let _ = std::fs::remove_file(icon_file());
    for list in mimeapps_lists() {
        forget_in_mimeapps(&list);
    }
    let _ = run_quiet("update-desktop-database", &[&applications_dir().to_string_lossy()]);
    forget_default_cache();
}

/// Take ProtoPDF out of every entry of a mimeapps.list file, leaving the rest as it was.
fn forget_in_mimeapps(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let mut changed = false;
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let t = line.trim();
        if let Some((key, value)) = t.split_once('=').filter(|_| !t.starts_with('#') && !t.starts_with('[')) {
            let items: Vec<&str> = value.split(';').map(str::trim).filter(|v| !v.is_empty()).collect();
            if items.contains(&DESKTOP_ID) {
                changed = true;
                let kept: Vec<&str> = items.into_iter().filter(|v| *v != DESKTOP_ID).collect();
                if !kept.is_empty() {
                    out.push_str(&format!("{}={};\n", key.trim(), kept.join(";")));
                }
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if changed {
        let _ = write_atomic(path, out.as_bytes());
    }
}

/// The mimeapps.list files that can hold the user's default, most important first.
fn mimeapps_lists() -> Vec<PathBuf> {
    let config = config_home();
    let mut lists: Vec<PathBuf> = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| config.join(format!("{}-mimeapps.list", d.to_ascii_lowercase())))
        .collect();
    lists.push(config.join("mimeapps.list"));
    // Older location, still read by some desktops.
    lists.push(applications_dir().join("mimeapps.list"));
    lists
}

/// The desktop entry the user chose for PDF files, if any.
fn user_default() -> Option<String> {
    for list in mimeapps_lists() {
        let Ok(text) = std::fs::read_to_string(&list) else { continue };
        let mut in_defaults = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                in_defaults = line == "[Default Applications]";
            } else if in_defaults {
                if let Some(value) = line.strip_prefix("application/pdf=") {
                    if let Some(first) = value.split(';').map(str::trim).find(|v| !v.is_empty()) {
                        return Some(first.to_string());
                    }
                }
            }
        }
    }
    None
}

static DEFAULT_CACHE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

fn forget_default_cache() {
    if let Ok(mut c) = DEFAULT_CACHE.lock() {
        *c = None;
    }
}

/// Whether PDF files currently open with this program. Asked every frame
/// while the File menu is open, so the answer is kept for a few seconds.
pub fn is_default() -> bool {
    let mut cache = DEFAULT_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, value)) = *cache {
        if at.elapsed() < Duration::from_secs(3) {
            return value;
        }
    }
    let value = user_default().as_deref() == Some(DESKTOP_ID) && desktop_file().exists();
    *cache = Some((Instant::now(), value));
    value
}

/// Open a web or mail link in the default browser or mail program.
/// Anything else (files, programs, other schemes) is refused.
pub fn open_url(url: &str) -> bool {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")) {
        return false;
    }
    match Command::new("xdg-open").arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(mut child) => {
            // Collect its exit status so it does not linger as a zombie process.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_quoting() {
        assert_eq!(exec_arg("/home/a/.local/bin/protopdf"), "/home/a/.local/bin/protopdf");
        assert_eq!(exec_arg("/opt/my apps/pro$pdf"), "\"/opt/my apps/pro\\\\$pdf\"");
        assert_eq!(exec_arg("/x/100%"), "/x/100%%");
    }

    #[test]
    fn mimeapps_cleanup() {
        let dir = std::env::temp_dir().join(format!("protopdf-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let list = dir.join("mimeapps.list");
        std::fs::write(
            &list,
            "[Default Applications]\napplication/pdf=protopdf.desktop\ntext/plain=gedit.desktop\n\n[Added Associations]\napplication/pdf=protopdf.desktop;org.gnome.Evince.desktop;\n",
        )
        .unwrap();
        forget_in_mimeapps(&list);
        let text = std::fs::read_to_string(&list).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(text, "[Default Applications]\ntext/plain=gedit.desktop\n\n[Added Associations]\napplication/pdf=org.gnome.Evince.desktop;\n");
    }
}
