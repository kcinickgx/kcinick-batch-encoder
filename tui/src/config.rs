//! Config persistence in `~/.config/fast6/config.json`.
//! Replaces the Windows registry (HKCU) the GUI uses. Same keys:
//! `ffmpeg`, `codec`, `mode`, `daemon_addr`, `token`.
use std::collections::BTreeMap;
use std::path::PathBuf;

fn dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("fast6")
}

fn file() -> PathBuf {
    dir().join("config.json")
}

fn read_all() -> BTreeMap<String, String> {
    std::fs::read_to_string(file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Reads a value. None if it doesn't exist or is empty.
pub fn load(key: &str) -> Option<String> {
    read_all().get(key).filter(|s| !s.is_empty()).cloned()
}

/// Saves a value (silent on errors, like reg_save).
pub fn save(key: &str, val: &str) {
    let mut map = read_all();
    map.insert(key.to_string(), val.to_string());
    let _ = std::fs::create_dir_all(dir());
    if let Ok(s) = serde_json::to_string_pretty(&map) {
        let _ = std::fs::write(file(), s);
    }
}
