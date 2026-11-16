//! Persistent settings: `%APPDATA%\vltracker\config.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Machine-local data (icon cache, win-probability model): `%LOCALAPPDATA%\vltracker`.
pub fn local_data_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("XDG_CACHE_HOME"))?;
    Some(PathBuf::from(base).join("vltracker"))
}

#[derive(Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub henrik_api_key: String,
    #[serde(default)]
    pub overlay: OverlayConfig,
}

/// Overlay box positions are top-left corners as fractions of the primary monitor, so one
/// calibration holds across resolutions with the same aspect ratio.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayConfig {
    /// Toggles the overlay. Windows reserves the key while the app runs.
    pub hotkey: String,
    /// Toggles calibration (drag boxes into place; saved on exit).
    pub calibrate_hotkey: String,
    /// One box per teammate row in agent select, top to bottom.
    pub agent_select: Vec<[f32; 2]>,
    /// One box per player card on the loading screen, left to right.
    pub loading_ally: Vec<[f32; 2]>,
    pub loading_enemy: Vec<[f32; 2]>,
    /// `ShooterGame.log` substrings marking the first buy phase, i.e. the loading screen ending.
    pub round_start_markers: Vec<String>,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        let column = |x: f32, y0: f32, dy: f32| (0..5).map(|i| [x, y0 + dy * i as f32]).collect();
        let row = |y: f32| (0..5).map(|i| [0.06 + 0.19 * i as f32, y]).collect();
        Self {
            hotkey: "F8".into(),
            calibrate_hotkey: "Shift+F8".into(),
            agent_select: column(0.16, 0.22, 0.095),
            loading_ally: row(0.12),
            loading_enemy: row(0.62),
            // From Overwolf's Valorant log parser: first `Gameplay started` line = round 1 buy phase.
            round_start_markers: vec!["Gameplay started at local time".into()],
        }
    }
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
