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
    /// Switches the overlay between click-through and clickable (showing it if hidden).
    /// Windows reserves the key while the app runs.
    pub hotkey: String,
    /// Toggles calibration (drag boxes into place; saved on exit).
    pub calibrate_hotkey: String,
    /// Layout generation the positions below belong to. Older saved positions are replaced with
    /// the current defaults on load. Missing in old files, hence 0.
    #[serde(default)]
    pub layout_version: u32,
    /// One box under each agent-select card, left to right.
    pub agent_select: Vec<[f32; 2]>,
    /// Bottom-left corner of the agent-select detail card.
    pub agent_select_card: [f32; 2],
    /// One box per loading-screen player card, top to bottom; allies left, enemies right.
    pub loading_ally: Vec<[f32; 2]>,
    pub loading_enemy: Vec<[f32; 2]>,
    /// `ShooterGame.log` substrings marking the first buy phase, i.e. the loading screen ending.
    pub round_start_markers: Vec<String>,
}

/// Bump when the default positions change.
const LAYOUT_VERSION: u32 = 2;

/// Fraction of a 1920×1080 reference screen.
fn at(x: f32, y: f32) -> [f32; 2] {
    [x / 1920.0, y / 1080.0]
}

impl Default for OverlayConfig {
    /// Positions measured on Valorant's 1920×1080 agent-select and loading screens.
    fn default() -> Self {
        // Loading-screen cards start at y = 177 every ~167.5 px; boxes sit on each card's banner.
        let loading = |x: f32| (0..5).map(|i| at(x, 205.0 + 167.5 * i as f32)).collect();
        Self {
            hotkey: "F8".into(),
            calibrate_hotkey: "Shift+F8".into(),
            layout_version: LAYOUT_VERSION,
            // Agent-select cards are 126 px wide every 136 px from x = 620; the free strip below
            // them starts at y ≈ 1040 (the LOCK IN button covers the space above).
            agent_select: (0..5)
                .map(|i| at(620.0 + 136.0 * i as f32, 1040.0))
                .collect(),
            // Right of the card row, clear of LOCK IN and the "enemy team locked in" indicator.
            agent_select_card: at(1305.0, 1020.0),
            loading_ally: loading(312.0),
            loading_enemy: loading(1357.0),
            // From Overwolf's Valorant log parser: first `Gameplay started` line = round 1 buy phase.
            round_start_markers: vec!["Gameplay started at local time".into()],
        }
    }
}

impl OverlayConfig {
    pub fn reset_positions(&mut self) {
        let d = Self::default();
        self.layout_version = d.layout_version;
        self.agent_select = d.agent_select;
        self.agent_select_card = d.agent_select_card;
        self.loading_ally = d.loading_ally;
        self.loading_enemy = d.loading_enemy;
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
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .unwrap_or_default()
            .migrated()
    }

    /// Replaces overlay positions saved by an older layout generation with the current defaults.
    fn migrated(mut self) -> Self {
        if self.overlay.layout_version < LAYOUT_VERSION {
            self.overlay.reset_positions();
        }
        self
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path().ok_or_else(|| std::io::Error::other("APPDATA is not set"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn migrated(json: &str) -> Config {
        serde_json::from_str::<Config>(json).unwrap().migrated()
    }

    #[test]
    fn old_saved_positions_are_replaced_but_settings_kept() {
        // Written by an earlier build when the user saved their API key.
        let old =
            r#"{"henrik_api_key":"HDEV-x","overlay":{"hotkey":"F9","agent_select":[[0.16,0.22]]}}"#;
        let config = migrated(old);
        assert_eq!(config.henrik_api_key, "HDEV-x");
        assert_eq!(config.overlay.hotkey, "F9", "user settings survive");
        assert_eq!(
            config.overlay.agent_select,
            OverlayConfig::default().agent_select
        );

        // A current-version calibration is left alone.
        let current = format!(
            r#"{{"overlay":{{"layout_version":{LAYOUT_VERSION},"agent_select":[[0.5,0.5]]}}}}"#
        );
        assert_eq!(migrated(&current).overlay.agent_select, vec![[0.5, 0.5]]);
    }
}
