//! Persistent settings: `%APPDATA%\vltracker\config.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub henrik_api_key: String,
}

impl Config {
    fn path() -> Option<PathBuf> {
        let base = std::env::var_os("APPDATA").or_else(|| std::env::var_os("XDG_CONFIG_HOME"))?;
        Some(PathBuf::from(base).join("vltracker").join("config.json"))
    }

    /// Missing or unreadable config falls back to defaults; the app stays usable.
    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path().ok_or_else(|| std::io::Error::other("APPDATA is not set"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)
    }
}
