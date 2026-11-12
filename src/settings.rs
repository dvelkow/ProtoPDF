//! Preferences kept between runs: the theme, which side panels are open and
//! the recently opened files. Stored as plain `key=value` lines in
//! `%APPDATA%\ProtoPDF\settings.txt` on Windows and
//! `~/.config/ProtoPDF/settings.txt` on Linux.

use std::path::{Path, PathBuf};

const MAX_RECENT: usize = 8;

pub struct Settings {
    pub light: bool,
    pub sidebar: bool,
    pub panel: bool,
    pub recent: Vec<PathBuf>,
    /// False when the theme was forced from the command line: nothing is written then.
    pub persist: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { light: false, sidebar: true, panel: true, recent: Vec::new(), persist: true }
    }
}

fn file() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    Some(base?.join("ProtoPDF").join("settings.txt"))
}

impl Settings {
    pub fn load() -> Self {
        let mut s = Settings::default();
        let Some(text) = file().and_then(|f| std::fs::read_to_string(f).ok()) else { return s };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let on = value.trim() == "1";
            match key.trim() {
                "light" => s.light = on,
                "sidebar" => s.sidebar = on,
                "panel" => s.panel = on,
                "recent" if !value.is_empty() && s.recent.len() < MAX_RECENT => s.recent.push(PathBuf::from(value)),
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        if !self.persist {
            return;
        }
        let Some(path) = file() else { return };
        let flag = |b: bool| if b { "1" } else { "0" };
        let mut out = format!("light={}\nsidebar={}\npanel={}\n", flag(self.light), flag(self.sidebar), flag(self.panel));
        for r in &self.recent {
            if let Some(s) = r.to_str() {
                out.push_str("recent=");
                out.push_str(s);
                out.push('\n');
            }
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, out);
    }

    /// Put a file at the top of the recent list.
    pub fn add_recent(&mut self, path: &Path) {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        self.recent.retain(|p| p != &path);
        self.recent.insert(0, path);
        self.recent.truncate(MAX_RECENT);
        self.save();
    }
}
