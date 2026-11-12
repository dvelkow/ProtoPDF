//! Registers ProtoPDF with Windows as a program that can open PDF files.
//!
//! Everything is written under HKEY_CURRENT_USER, so no administrator rights
//! are needed. Windows does not let a program appoint itself as the default:
//! that last step is the user's choice in Settings, which `open_settings` opens
//! on the right page.

use std::path::Path;
use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

pub const APP: &str = "ProtoPDF";
pub const PROG_ID: &str = "ProtoPDF.Document";

pub fn register(exe: &Path) -> Result<(), String> {
    let exe = exe.to_string_lossy().to_string();
    let open = format!("\"{exe}\" \"%1\"");
    let icon = format!("\"{exe}\",0");
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let set = |path: &str, name: &str, value: &str| -> Result<(), String> {
        let (key, _) = hkcu.create_subkey(path).map_err(|e| format!("{path}: {e}"))?;
        key.set_value(name, &value).map_err(|e| format!("{path}: {e}"))
    };

    // The file type handled by this program.
    let class = format!("Software\\Classes\\{PROG_ID}");
    set(&class, "", "PDF document")?;
    set(&class, "FriendlyTypeName", "PDF document")?;
    set(&format!("{class}\\DefaultIcon"), "", &icon)?;
    set(&format!("{class}\\shell\\open\\command"), "", &open)?;
    set(&format!("{class}\\shell\\print\\command"), "", &format!("\"{exe}\" --print \"%1\""))?;
    set(&format!("{class}\\Application"), "ApplicationName", APP)?;
    set(&format!("{class}\\Application"), "ApplicationDescription", "PDF reader with page editing and printing")?;
    set(&format!("{class}\\Application"), "ApplicationIcon", &icon)?;

    // Offer it in "Open with" for .pdf.
    set("Software\\Classes\\.pdf\\OpenWithProgids", PROG_ID, "")?;
    let app = "Software\\Classes\\Applications\\ProtoPDF.exe";
    set(app, "FriendlyAppName", APP)?;
    set(&format!("{app}\\DefaultIcon"), "", &icon)?;
    set(&format!("{app}\\shell\\open\\command"), "", &open)?;
    set(&format!("{app}\\SupportedTypes"), ".pdf", "")?;

    // List it under Settings > Apps > Default apps.
    let caps = "Software\\ProtoPDF\\Capabilities";
    set(caps, "ApplicationName", APP)?;
    set(caps, "ApplicationDescription", "PDF reader with page editing and printing")?;
    set(caps, "ApplicationIcon", &icon)?;
    set(&format!("{caps}\\FileAssociations"), ".pdf", PROG_ID)?;
    set("Software\\RegisteredApplications", APP, caps)?;

    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
    Ok(())
}

/// Remove everything `register` wrote.
pub fn unregister() {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for key in [
        format!("Software\\Classes\\{PROG_ID}"),
        "Software\\Classes\\Applications\\ProtoPDF.exe".to_string(),
        "Software\\ProtoPDF".to_string(),
    ] {
        let _ = hkcu.delete_subkey_all(key);
    }
    if let Ok(k) = hkcu.open_subkey_with_flags("Software\\Classes\\.pdf\\OpenWithProgids", winreg::enums::KEY_SET_VALUE) {
        let _ = k.delete_value(PROG_ID);
    }
    if let Ok(k) = hkcu.open_subkey_with_flags("Software\\RegisteredApplications", winreg::enums::KEY_SET_VALUE) {
        let _ = k.delete_value(APP);
    }
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

/// Whether Windows currently opens .pdf files with this program.
pub fn is_default() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    hkcu.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\.pdf\\UserChoice")
        .and_then(|k| k.get_value::<String, _>("ProgId"))
        .map(|p| p.eq_ignore_ascii_case(PROG_ID))
        .unwrap_or(false)
}

/// Open a web or mail link in the default browser or mail program.
/// Anything else (files, programs, other schemes) is refused.
pub fn open_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")) {
        return false;
    }
    let target = HSTRING::from(url.trim());
    let result = unsafe { ShellExecuteW(None, w!("open"), &target, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    result.0 as usize > 32
}

/// Open the Windows "Default apps" page for this program.
pub fn open_settings() {
    let url = HSTRING::from(format!("ms-settings:defaultapps?registeredAppUser={APP}"));
    unsafe {
        ShellExecuteW(None, w!("open"), &url, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}
