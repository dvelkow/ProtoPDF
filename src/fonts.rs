//! Finding font files installed on the computer.
//!
//! Callers ask for a list of file names in order of preference. On Windows
//! they are looked up in the Windows Fonts folder. On Linux every usual font
//! folder is searched, so the Windows names are found too when the Microsoft
//! core fonts are installed, and otherwise the next names on the list are
//! used, normally the Liberation fonts, which have the same letter widths as
//! Arial, Times New Roman and Courier New and include Cyrillic.

/// The contents of the first font on the list that is installed.
pub fn find(names: &[&str]) -> Option<Vec<u8>> {
    names.iter().find_map(|name| std::fs::read(path(name)?).ok())
}

#[cfg(windows)]
fn path(name: &str) -> Option<std::path::PathBuf> {
    let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
    Some(std::path::PathBuf::from(windir).join("Fonts").join(name))
}

#[cfg(not(windows))]
fn path(name: &str) -> Option<std::path::PathBuf> {
    index().get(&name.to_ascii_lowercase()).cloned()
}

/// Every font file under the font folders, by lower case file name. Built once.
#[cfg(not(windows))]
fn index() -> &'static std::collections::HashMap<String, std::path::PathBuf> {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    fn walk(dir: &Path, depth: u32, map: &mut HashMap<String, PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        // Sorted, and later entries win: "liberation2" (the newer release) replaces "liberation".
        entries.sort();
        for p in entries {
            // metadata() follows links, which font packages use a lot.
            let Ok(meta) = std::fs::metadata(&p) else { continue };
            if meta.is_dir() {
                if depth < 6 {
                    walk(&p, depth + 1, map);
                }
            } else if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                let lower = name.to_ascii_lowercase();
                if lower.ends_with(".ttf") || lower.ends_with(".otf") || lower.ends_with(".ttc") {
                    map.insert(lower, p);
                }
            }
        }
    }

    static INDEX: OnceLock<HashMap<String, PathBuf>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
        let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());

        // System folders first, the user's own last, so the user's fonts win.
        let mut dirs: Vec<PathBuf> = data_dirs.split(':').rev().filter(|d| !d.is_empty()).map(|d| Path::new(d).join("fonts")).collect();
        for fixed in ["/usr/share/fonts", "/usr/local/share/fonts"] {
            if !dirs.iter().any(|d| d == Path::new(fixed)) {
                dirs.insert(0, PathBuf::from(fixed));
            }
        }
        dirs.extend(data_home.map(|d| d.join("fonts")));
        dirs.extend(home.map(|h| h.join(".fonts")));

        let mut map = HashMap::new();
        for d in dirs {
            walk(&d, 0, &mut map);
        }
        map
    })
}

#[cfg(all(test, not(windows)))]
mod tests {
    #[test]
    fn finds_a_sans_font() {
        // Any desktop Linux has at least one of these.
        assert!(super::find(&["arial.ttf", "LiberationSans-Regular.ttf", "DejaVuSans.ttf"]).is_some());
    }
}
