//! Static game data from valorant-api.com plus a downscaling, disk-cached icon loader.
//!
//! Agent `displayIcon`s are 1024x1024 PNGs (~4 MB decoded each). Instead of handing those to
//! egui's image cache, icons are decoded once, shrunk to `ICON_PX`, and cached on disk, so the
//! resident cost per icon is a ~16 KB texture.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;

use eframe::egui;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::model::{Error, IconKey, Result};

const AGENTS_URL: &str = "https://valorant-api.com/v1/agents?isPlayableCharacter=true";
const TIERS_URL: &str = "https://valorant-api.com/v1/competitivetiers";
const SEASONS_URL: &str = "https://valorant-api.com/v1/seasons";
const COMP_SEASONS_URL: &str = "https://valorant-api.com/v1/seasons/competitive";
const VERSION_URL: &str = "https://valorant-api.com/v1/version";
const MAPS_URL: &str = "https://valorant-api.com/v1/maps";

/// Square edge of cached icons: 32 logical px at 2x DPI scaling.
const ICON_PX: u32 = 64;

pub struct Agent {
    pub name: String,
    pub icon: String,
    /// "Duelist", "Initiator", "Controller" or "Sentinel".
    pub role: Option<String>,
}

/// A named map area at a point in game-world coordinates (the same space as match positions).
pub struct Callout {
    /// Super-region: "A", "B", "C", "Mid", "Attacker Side", "Defender Side".
    pub zone: String,
    /// e.g. "Main", "Site", "Lobby".
    pub region: String,
    x: f32,
    y: f32,
}

impl Callout {
    pub fn is_site(&self) -> bool {
        matches!(self.zone.as_str(), "A" | "B" | "C")
    }

    /// e.g. "A Main".
    pub fn label(&self) -> String {
        format!("{} {}", self.zone, self.region)
    }
}

pub struct Tier {
    pub name: String,
    pub icon: Option<String>,
}

pub struct Act {
    /// e.g. "V26 // ACT IV".
    pub label: String,
    /// RFC 3339; sorts chronologically as a string.
    pub start: String,
    /// Tier table this act was ranked on; `None` means the current table.
    table: Option<String>,
}

pub struct Assets {
    /// Keyed by lowercase agent UUID.
    pub agents: HashMap<String, Agent>,
    /// Keyed by tier index from the current episode's tier table.
    pub tiers: HashMap<u32, Tier>,
    /// Keyed by lowercase act UUID (PD `SeasonID`, HenrikDev `season.id`).
    pub acts: HashMap<String, Act>,
    /// Older tier tables mapped onto current tier numbers, keyed by table UUID.
    tier_maps: HashMap<String, HashMap<u32, u32>>,
    /// `riotClientVersion`, used if the local game log has no version line.
    pub client_version: Option<String>,
    /// Callouts per map, keyed by lowercase map UUID (HenrikDev `metadata.map.id`).
    maps: HashMap<String, Vec<Callout>>,
}

#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentDto {
    uuid: String,
    display_name: String,
    display_icon: Option<String>,
    role: Option<RoleDto>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoleDto {
    display_name: String,
}

#[derive(Deserialize)]
struct MapDto {
    uuid: String,
    #[serde(default)]
    callouts: Option<Vec<CalloutDto>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CalloutDto {
    region_name: String,
    super_region_name: String,
    location: PointDto,
}

#[derive(Deserialize)]
struct PointDto {
    x: f32,
    y: f32,
}

#[derive(Deserialize)]
struct TierTableDto {
    uuid: String,
    tiers: Vec<TierDto>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeasonDto {
    uuid: String,
    display_name: String,
    title: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    parent_uuid: Option<String>,
    start_time: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompSeasonDto {
    season_uuid: String,
    competitive_tiers_uuid: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TierDto {
    tier: u32,
    tier_name: String,
    small_icon: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionDto {
    riot_client_version: String,
}

async fn get_data<T: DeserializeOwned>(http: &reqwest::Client, url: &str) -> Result<T> {
    let resp = http.get(url).send().await?.error_for_status()?;
    Ok(resp.json::<Envelope<T>>().await?.data)
}

impl Assets {
    pub async fn fetch(http: &reqwest::Client) -> Result<Self> {
        let (agents, tables, seasons, comp, maps, version) = tokio::join!(
            get_data::<Vec<AgentDto>>(http, AGENTS_URL),
            get_data::<Vec<TierTableDto>>(http, TIERS_URL),
            get_data::<Vec<SeasonDto>>(http, SEASONS_URL),
            get_data::<Vec<CompSeasonDto>>(http, COMP_SEASONS_URL),
            get_data::<Vec<MapDto>>(http, MAPS_URL),
            get_data::<VersionDto>(http, VERSION_URL),
        );

        let agents = agents?
            .into_iter()
            .filter_map(|a| {
                let icon = a.display_icon?;
                let agent = Agent { name: a.display_name, icon, role: a.role.map(|r| r.display_name) };
                Some((a.uuid.to_ascii_lowercase(), agent))
            })
            .collect();

        // Callouts only feed the entry breakdown; without them it is simply omitted.
        let maps = maps
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| {
                let callouts = m
                    .callouts?
                    .into_iter()
                    .map(|c| Callout {
                        zone: c.super_region_name,
                        region: c.region_name,
                        x: c.location.x,
                        y: c.location.y,
                    })
                    .collect();
                Some((m.uuid.to_ascii_lowercase(), callouts))
            })
            .collect();

        // Tables are ordered by episode; the last one is current.
        let mut tables = tables?;
        let current = tables.pop().ok_or(Error::Missing("competitive tier table"))?;
        let tier_maps = tables.iter().map(|t| (t.uuid.to_ascii_lowercase(), map_onto(t, &current))).collect();
        let tiers = current
            .tiers
            .into_iter()
            .map(|t| (t.tier, Tier { name: title_case(&t.tier_name), icon: t.small_icon }))
            .collect();

        // Act metadata is optional: without it, act history shows raw labels.
        let table_of: HashMap<String, String> = comp
            .unwrap_or_default()
            .into_iter()
            .map(|c| (c.season_uuid.to_ascii_lowercase(), c.competitive_tiers_uuid.to_ascii_lowercase()))
            .collect();
        let seasons = seasons.unwrap_or_default();
        let names: HashMap<&str, &str> = seasons.iter().map(|s| (s.uuid.as_str(), s.display_name.as_str())).collect();
        let acts = seasons
            .iter()
            .filter(|s| s.kind.as_deref() == Some("EAresSeasonType::Act"))
            .map(|s| {
                let parent = s.parent_uuid.as_deref().unwrap_or_default();
                // Newer acts carry a title ("V26 // ACT IV"); older ones are built from
                // upper-case names, so "EPISODE 4 ACT III" becomes "Episode 4 Act III".
                let label = s.title.clone().unwrap_or_else(|| {
                    title_case(&format!("{} {}", names.get(parent).copied().unwrap_or_default(), s.display_name))
                });
                // Competitive seasons are keyed by the act or by its parent episode.
                let table = table_of
                    .get(&s.uuid.to_ascii_lowercase())
                    .or_else(|| table_of.get(&parent.to_ascii_lowercase()))
                    .filter(|t| **t != current.uuid.to_ascii_lowercase())
                    .cloned();
                let act = Act { label, start: s.start_time.clone().unwrap_or_default(), table };
                (s.uuid.to_ascii_lowercase(), act)
            })
            .collect();

        Ok(Self {
            agents,
            tiers,
            acts,
            tier_maps,
            client_version: version.ok().map(|v| v.riot_client_version),
            maps,
        })
    }

    /// Converts a tier number recorded during `act` to the current table's numbering.
    pub fn normalize_tier(&self, act: &str, tier: u32) -> u32 {
        self.acts
            .get(act)
            .and_then(|a| a.table.as_ref())
            .and_then(|t| self.tier_maps.get(t))
            .and_then(|m| m.get(&tier).copied())
            .unwrap_or(tier)
    }

    /// Nearest named area to a game-world position on `map`.
    pub fn callout(&self, map: &str, x: f32, y: f32) -> Option<&Callout> {
        self.maps.get(map)?.iter().min_by(|a, b| {
            let d = |c: &Callout| (c.x - x).powi(2) + (c.y - y).powi(2);
            d(a).total_cmp(&d(b))
        })
    }

    pub fn agent_role(&self, uuid: &str) -> Option<&str> {
        self.agents.get(uuid).and_then(|a| a.role.as_deref())
    }

    pub fn icon_url(&self, key: &IconKey) -> Option<&str> {
        match key {
            IconKey::Agent(uuid) => self.agents.get(uuid).map(|a| a.icon.as_str()),
            IconKey::Rank(tier) => self.tiers.get(tier).and_then(|t| t.icon.as_deref()),
        }
    }

    pub fn agent_name(&self, uuid: &str) -> Option<&str> {
        self.agents.get(uuid).map(|a| a.name.as_str())
    }

    pub fn tier_name(&self, tier: u32) -> Option<&str> {
        self.tiers.get(&tier).map(|t| t.name.as_str())
    }
}

/// Maps an old table's tiers onto the current one by name. Episode 2's single "IMMORTAL" falls back
/// to the first current tier sharing its first word ("IMMORTAL 1").
fn map_onto(old: &TierTableDto, current: &TierTableDto) -> HashMap<u32, u32> {
    old.tiers
        .iter()
        .filter_map(|o| {
            let first_word = o.tier_name.split_whitespace().next()?;
            let hit = current.tiers.iter().find(|c| c.tier_name == o.tier_name).or_else(|| {
                current.tiers.iter().find(|c| c.tier_name.split_whitespace().next() == Some(first_word))
            })?;
            Some((o.tier, hit.tier))
        })
        .collect()
}

/// "IMMORTAL 3" → "Immortal 3"; roman numerals stay upper-case ("EPISODE 4 ACT III" →
/// "Episode 4 Act III").
fn title_case(s: &str) -> String {
    s.split(' ')
        .map(|word| {
            if !word.is_empty() && word.chars().all(|c| matches!(c, 'I' | 'V' | 'X')) {
                return word.to_owned();
            }
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn icon_cache_path(url: &str) -> Option<PathBuf> {
    let base = crate::config::local_data_dir()?;
    // URLs embed the asset UUID, so a changed asset gets a new file automatically.
    let file: String = url
        .trim_start_matches("https://")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    Some(base.join("icons").join(file))
}

/// Returns a small RGBA icon, downloading and shrinking it on first use.
pub async fn load_icon(http: &reqwest::Client, url: &str) -> Result<egui::ColorImage> {
    let path = icon_cache_path(url);
    if let Some(img) = path.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|b| decode(&b).ok()) {
        return Ok(img);
    }

    let bytes = http.get(url).send().await?.error_for_status()?.bytes().await?;
    let (small_png, image) = tokio::task::spawn_blocking(move || -> Result<_> {
        let thumb = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)?
            .thumbnail(ICON_PX, ICON_PX)
            .into_rgba8();
        let mut png = Vec::new();
        thumb.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)?;
        let size = [thumb.width() as usize, thumb.height() as usize];
        Ok((png, egui::ColorImage::from_rgba_unmultiplied(size, thumb.as_raw())))
    })
    .await
    .map_err(|_| Error::Missing("icon decoder"))??;

    if let Some(path) = path {
        // Cache write failures only cost a re-download next launch.
        let _ = path.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(path, small_png);
    }
    Ok(image)
}

fn decode(png: &[u8]) -> Result<egui::ColorImage> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)?.into_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()))
}

#[cfg(test)]
mod tests {
    use super::title_case;

    #[test]
    fn title_case_keeps_roman_numerals() {
        assert_eq!(title_case("EPISODE 4 ACT III"), "Episode 4 Act III");
        assert_eq!(title_case("IMMORTAL 3"), "Immortal 3");
        assert_eq!(title_case("IRON 1"), "Iron 1");
    }
}
