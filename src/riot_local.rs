//! Riot Client integration.
//!
//! * Local API (`https://127.0.0.1:{port}`, self-signed cert, Basic `riot:{password}`): auth tokens,
//!   presence. Polled cheaply to learn the session loop state.
//! * Remote GLZ (`glz-{region}-1.{shard}.a.pvp.net`): pregame / core-game rosters. Only hit when
//!   presence says we are in a match.
//! * Remote PD (`pd.{shard}.a.pvp.net`): batched name lookup and per-player MMR.
//!
//! Remote calls use the user's own client tokens; no Riot developer key is involved.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;

use base64::Engine;
use parking_lot::Mutex;
use reqwest::{RequestBuilder, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::OnceCell;

use crate::model::{ActRank, Error, Rank, RankProfile, Result, Roster, Side, Slot};

/// Base64 JSON platform descriptor every Riot client sends (PC / Windows 10).
const CLIENT_PLATFORM: &str = "ew0KCSJwbGF0Zm9ybVR5cGUiOiAiUEMiLA0KCSJwbGF0Zm9ybU9TIjogIldpbmRvd3MiLA0KCSJwbGF0Zm9ybU9TVmVyc2lvbiI6ICIxMC4wLjE5MDQyLjEuMjU2LjY0Yml0IiwNCgkicGxhdGZvcm1DaGlwc2V0IjogIlVua25vd24iDQp9";

fn local_app_data() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

/// `name:pid:port:password:protocol`
#[derive(Clone, PartialEq, Eq)]
pub struct Lockfile {
    pub port: u16,
    pub password: String,
}

impl Lockfile {
    pub fn read() -> Option<Self> {
        let path = local_app_data()?.join(r"Riot Games\Riot Client\Config\lockfile");
        Self::parse(&std::fs::read_to_string(path).ok()?)
    }

    fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.trim().split(':');
        let (_name, _pid) = (parts.next()?, parts.next()?);
        let port = parts.next()?.parse().ok()?;
        let password = parts.next()?.to_owned();
        Some(Self { port, password })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoopState {
    /// Riot Client is up; Valorant is in menus or not running.
    Menus,
    Pregame,
    InGame,
    /// Presence was present but unreadable (format change); fall back to probing GLZ.
    Unknown,
}

#[derive(Deserialize)]
struct EntitlementsDto {
    #[serde(rename = "accessToken")]
    access_token: String,
    token: String,
    subject: String,
}

struct Tokens {
    access: String,
    entitlements: String,
}

#[derive(Deserialize)]
struct PresencesDto {
    presences: Vec<PresenceDto>,
}

#[derive(Deserialize)]
struct PresenceDto {
    puuid: String,
    #[serde(default)]
    product: String,
    #[serde(default)]
    private: Option<String>,
}

#[derive(Deserialize)]
struct MatchIdDto {
    #[serde(rename = "MatchID")]
    match_id: String,
}

#[derive(Deserialize, Default)]
struct IdentityDto {
    #[serde(rename = "Incognito", default)]
    incognito: bool,
}

#[derive(Deserialize)]
struct PregameMatchDto {
    #[serde(rename = "AllyTeam")]
    ally_team: Option<PregameTeamDto>,
}

#[derive(Deserialize)]
struct PregameTeamDto {
    #[serde(rename = "Players", default)]
    players: Vec<PregamePlayerDto>,
}

#[derive(Deserialize)]
struct PregamePlayerDto {
    #[serde(rename = "Subject")]
    subject: String,
    #[serde(rename = "CharacterID", default)]
    character_id: String,
    #[serde(rename = "CharacterSelectionState", default)]
    selection_state: String,
    #[serde(rename = "PlayerIdentity", default)]
    identity: IdentityDto,
}

#[derive(Deserialize)]
struct CoreMatchDto {
    #[serde(rename = "Players", default)]
    players: Vec<CorePlayerDto>,
}

#[derive(Deserialize)]
struct CorePlayerDto {
    #[serde(rename = "Subject")]
    subject: String,
    #[serde(rename = "TeamID", default)]
    team_id: String,
    #[serde(rename = "CharacterID", default)]
    character_id: String,
    #[serde(rename = "PlayerIdentity", default)]
    identity: IdentityDto,
}

#[derive(Deserialize)]
struct NameDto {
    #[serde(rename = "Subject")]
    subject: String,
    #[serde(rename = "GameName", default)]
    game_name: String,
    #[serde(rename = "TagLine", default)]
    tag_line: String,
}

#[derive(Deserialize)]
struct ContentDto {
    #[serde(rename = "Seasons", default)]
    seasons: Vec<SeasonDto>,
}

#[derive(Deserialize)]
struct SeasonDto {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Type", default)]
    kind: String,
    #[serde(rename = "IsActive", default)]
    is_active: bool,
}

#[derive(Deserialize)]
struct MmrDto {
    #[serde(rename = "QueueSkills", default)]
    queue_skills: HashMap<String, QueueSkillDto>,
}

#[derive(Deserialize)]
struct QueueSkillDto {
    #[serde(rename = "SeasonalInfoBySeasonID", default)]
    seasonal: Option<HashMap<String, SeasonalInfoDto>>,
}

#[derive(Deserialize)]
struct SeasonalInfoDto {
    #[serde(rename = "CompetitiveTier", default)]
    tier: u32,
    #[serde(rename = "RankedRating", default)]
    rr: u32,
    #[serde(rename = "NumberOfWinsWithPlacements", default)]
    wins: u32,
    #[serde(rename = "NumberOfGames", default)]
    games: u32,
    /// Tier (as a string key) → wins at that tier.
    #[serde(rename = "WinsByTier", default)]
    wins_by_tier: Option<HashMap<String, u32>>,
}

#[derive(Deserialize)]
struct RegionLocaleDto {
    region: String,
}

/// A logged-in Riot Client session. Shared across fetch tasks behind an `Arc`.
pub struct RiotSession {
    local: reqwest::Client,
    remote: reqwest::Client,
    local_base: String,
    password: String,
    pub puuid: String,
    /// Region (na, latam, br, eu, ap, kr). Also the HenrikDev `affinity`.
    pub region: String,
    glz: String,
    pd: String,
    shared: String,
    client_version: String,
    tokens: Mutex<Tokens>,
    season: OnceCell<String>,
}

impl RiotSession {
    /// Authenticates against the local client and resolves region, shard, and client version.
    /// Fails while the Riot Client is starting or the user is not logged in.
    pub async fn connect(
        lock: &Lockfile,
        local: reqwest::Client,
        remote: reqwest::Client,
        fallback_version: Option<&str>,
    ) -> Result<Self> {
        let local_base = format!("https://127.0.0.1:{}", lock.port);
        let ent: EntitlementsDto = local_get(&local, &local_base, &lock.password, "/entitlements/v1/token").await?;

        let log = read_game_log();
        let (region, shard) = match log.glz {
            Some(rs) => rs,
            None => {
                let rl: RegionLocaleDto =
                    local_get(&local, &local_base, &lock.password, "/riotclient/region-locale").await?;
                let region = rl.region.trim_end_matches(|c: char| c.is_ascii_digit()).to_ascii_lowercase();
                let shard = shard_for(&region).ok_or(Error::Missing("Riot shard"))?.to_owned();
                (region, shard)
            }
        };
        let client_version = log
            .version
            .or_else(|| fallback_version.map(str::to_owned))
            .ok_or(Error::Missing("client version"))?;

        Ok(Self {
            glz: format!("https://glz-{region}-1.{shard}.a.pvp.net"),
            pd: format!("https://pd.{shard}.a.pvp.net"),
            shared: format!("https://shared.{shard}.a.pvp.net"),
            local,
            remote,
            local_base,
            password: lock.password.clone(),
            puuid: ent.subject,
            region,
            client_version,
            tokens: Mutex::new(Tokens { access: ent.access_token, entitlements: ent.token }),
            season: OnceCell::new(),
        })
    }

    pub async fn loop_state(&self) -> Result<LoopState> {
        let dto: PresencesDto = local_get(&self.local, &self.local_base, &self.password, "/chat/v4/presences").await?;
        let presence = dto.presences.into_iter().find(|p| p.puuid == self.puuid && p.product == "valorant");
        Ok(match presence.and_then(|p| p.private) {
            Some(private) => parse_loop_state(&private),
            None => LoopState::Menus,
        })
    }

    pub async fn pregame_match_id(&self) -> Result<Option<String>> {
        let url = format!("{}/pregame/v1/players/{}", self.glz, self.puuid);
        optional(self.remote_json::<MatchIdDto>(|c| c.get(&url)).await).map(|o| o.map(|m| m.match_id))
    }

    pub async fn core_match_id(&self) -> Result<Option<String>> {
        let url = format!("{}/core-game/v1/players/{}", self.glz, self.puuid);
        optional(self.remote_json::<MatchIdDto>(|c| c.get(&url)).await).map(|o| o.map(|m| m.match_id))
    }

    /// Agent select only exposes your own team.
    pub async fn pregame_roster(&self, match_id: &str) -> Result<Roster> {
        let url = format!("{}/pregame/v1/matches/{match_id}", self.glz);
        let dto: PregameMatchDto = self.remote_json(|c| c.get(&url)).await?;
        let slots = dto
            .ally_team
            .map(|t| t.players)
            .unwrap_or_default()
            .into_iter()
            .map(|p| {
                let is_self = p.subject == self.puuid;
                Slot {
                    side: Side::Ally,
                    agent: agent_id(&p.character_id),
                    locked: p.selection_state == "locked",
                    hidden: p.identity.incognito && !is_self,
                    is_self,
                    puuid: p.subject,
                }
            })
            .collect();
        Ok(Roster { match_id: match_id.to_owned(), ffa: false, slots })
    }

    pub async fn core_roster(&self, match_id: &str) -> Result<Roster> {
        let url = format!("{}/core-game/v1/matches/{match_id}", self.glz);
        let dto: CoreMatchDto = self.remote_json(|c| c.get(&url)).await?;
        let my_team = dto.players.iter().find(|p| p.subject == self.puuid).map(|p| p.team_id.clone());
        // Deathmatch gives every player their own team.
        let ffa = dto.players.iter().map(|p| p.team_id.as_str()).collect::<HashSet<_>>().len() > 2;
        let slots = dto
            .players
            .into_iter()
            .map(|p| {
                let is_self = p.subject == self.puuid;
                Slot {
                    side: if my_team.as_deref() == Some(p.team_id.as_str()) { Side::Ally } else { Side::Enemy },
                    agent: agent_id(&p.character_id),
                    locked: true,
                    hidden: p.identity.incognito && !is_self,
                    is_self,
                    puuid: p.subject,
                }
            })
            .collect();
        Ok(Roster { match_id: match_id.to_owned(), ffa, slots })
    }

    /// Resolves Riot IDs for many players in one request.
    pub async fn names(&self, puuids: &[&str]) -> Result<Vec<(String, String)>> {
        let url = format!("{}/name-service/v2/players", self.pd);
        let dto: Vec<NameDto> = self.remote_json(|c| c.put(&url).json(puuids)).await?;
        Ok(dto
            .into_iter()
            .filter(|n| !n.game_name.is_empty())
            .map(|n| (n.subject, format!("{}#{}", n.game_name, n.tag_line)))
            .collect())
    }

    /// Current-act rank plus every act this player has competitive games in.
    /// No entry for the current act means unranked.
    pub async fn rank(&self, puuid: &str) -> Result<RankProfile> {
        let season = self.current_season().await?;
        let url = format!("{}/mmr/v1/players/{puuid}", self.pd);
        let mut dto: MmrDto = self.remote_json(|c| c.get(&url)).await?;
        let seasonal = dto.queue_skills.remove("competitive").and_then(|q| q.seasonal).unwrap_or_default();
        let current = seasonal.get(season).map(|s| Rank { tier: s.tier, rr: s.rr }).unwrap_or(Rank { tier: 0, rr: 0 });
        let acts = seasonal
            .into_iter()
            .filter(|(_, s)| s.games > 0)
            .map(|(act, s)| {
                let peak = s.wins_by_tier.iter().flatten().filter_map(|(t, _)| t.parse().ok()).max();
                ActRank {
                    act: act.to_ascii_lowercase(),
                    peak_tier: peak.unwrap_or(0).max(s.tier),
                    tier: s.tier,
                    wins: s.wins,
                    games: s.games,
                }
            })
            .collect();
        Ok(RankProfile { current, acts })
    }

    /// Lowercase UUID of the active act, from Riot's content service.
    pub async fn current_act(&self) -> Result<String> {
        self.current_season().await.map(|s| s.to_ascii_lowercase())
    }

    async fn current_season(&self) -> Result<&String> {
        self.season
            .get_or_try_init(|| async {
                let url = format!("{}/content-service/v3/content", self.shared);
                let dto: ContentDto = self.remote_json(|c| c.get(&url)).await?;
                dto.seasons
                    .into_iter()
                    .find(|s| s.is_active && s.kind == "act")
                    .map(|s| s.id)
                    .ok_or(Error::Missing("active act"))
            })
            .await
    }

    async fn refresh_tokens(&self) -> Result<()> {
        let ent: EntitlementsDto =
            local_get(&self.local, &self.local_base, &self.password, "/entitlements/v1/token").await?;
        *self.tokens.lock() =
            Tokens { access: ent.access_token, entitlements: ent.token };
        Ok(())
    }

    /// Authenticated remote call. Tokens expire after ~1h; a 400/401 refreshes once and retries.
    async fn remote_json<T: DeserializeOwned>(&self, build: impl Fn(&reqwest::Client) -> RequestBuilder) -> Result<T> {
        for attempt in 0..2 {
            let (access, entitlements) = {
                let t = self.tokens.lock();
                (t.access.clone(), t.entitlements.clone())
            };
            let resp = build(&self.remote)
                .bearer_auth(access)
                .header("X-Riot-Entitlements-JWT", entitlements)
                .header("X-Riot-ClientPlatform", CLIENT_PLATFORM)
                .header("X-Riot-ClientVersion", &self.client_version)
                .send()
                .await?;
            match resp.status() {
                s if s.is_success() => return Ok(resp.json().await?),
                StatusCode::NOT_FOUND => return Err(Error::NotFound),
                StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED if attempt == 0 => self.refresh_tokens().await?,
                s => return Err(Error::Status(s)),
            }
        }
        Err(Error::Status(StatusCode::UNAUTHORIZED))
    }
}

async fn local_get<T: DeserializeOwned>(http: &reqwest::Client, base: &str, password: &str, path: &str) -> Result<T> {
    let resp = http.get(format!("{base}{path}")).basic_auth("riot", Some(password)).send().await?;
    match resp.status() {
        s if s.is_success() => Ok(resp.json().await?),
        StatusCode::NOT_FOUND => Err(Error::NotFound),
        s => Err(Error::Status(s)),
    }
}

fn optional<T>(r: Result<T>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(Error::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

fn agent_id(raw: &str) -> Option<String> {
    (!raw.is_empty()).then(|| raw.to_ascii_lowercase())
}

fn shard_for(region: &str) -> Option<&'static str> {
    Some(match region {
        "na" | "latam" | "br" => "na",
        "eu" => "eu",
        "ap" => "ap",
        "kr" => "kr",
        "pbe" => "pbe",
        _ => return None,
    })
}

/// Decodes the base64 JSON `private` presence blob.
fn parse_loop_state(private_b64: &str) -> LoopState {
    let Some(json) = base64::engine::general_purpose::STANDARD
        .decode(private_b64)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
    else {
        return LoopState::Unknown;
    };
    // Newer clients nest it under matchPresenceData; older ones keep it top-level.
    let state = json
        .pointer("/matchPresenceData/sessionLoopState")
        .or_else(|| json.get("sessionLoopState"))
        .and_then(|v| v.as_str());
    match state {
        Some("MENUS") => LoopState::Menus,
        Some("PREGAME") => LoopState::Pregame,
        Some("INGAME") => LoopState::InGame,
        _ => LoopState::Unknown,
    }
}

#[derive(Default)]
struct GameLog {
    /// (region, shard) from the first GLZ URL in the log.
    glz: Option<(String, String)>,
    version: Option<String>,
}

fn game_log_path() -> Option<PathBuf> {
    local_app_data().map(|p| p.join(r"VALORANT\Saved\Logs\ShooterGame.log"))
}

/// Follows lines appended to `ShooterGame.log` from a starting offset.
pub struct LogTail {
    path: PathBuf,
    offset: u64,
}

impl LogTail {
    /// Upper bound per read; the log grows by kilobytes per second at most.
    const MAX_READ: u64 = 8 << 20;

    /// Starts at the current end of the game log, so only future lines are seen.
    pub fn game_log_at_end() -> Option<Self> {
        let path = game_log_path()?;
        let offset = std::fs::metadata(&path).ok()?.len();
        Some(Self { path, offset })
    }

    /// Whether any complete line appended since the last call contains one of `markers`.
    pub fn saw_any(&mut self, markers: &[String]) -> bool {
        let Ok(mut file) = File::open(&self.path) else { return false };
        let Ok(len) = file.metadata().map(|m| m.len()) else { return false };
        if len < self.offset {
            self.offset = 0; // recreated by a game restart
        }
        let mut buf = Vec::new();
        if file.seek(SeekFrom::Start(self.offset)).is_err() || file.take(Self::MAX_READ).read_to_end(&mut buf).is_err()
        {
            return false;
        }
        // Consume complete lines only; a partially written last line is re-read next time.
        let Some(end) = buf.iter().rposition(|&b| b == b'\n') else { return false };
        self.offset += end as u64 + 1;
        let text = String::from_utf8_lossy(&buf[..end]);
        markers.iter().any(|m| text.contains(m.as_str()))
    }
}

/// Scans `ShooterGame.log` line by line (it can be many MB) and stops once both values are found.
fn read_game_log() -> GameLog {
    let mut out = GameLog::default();
    let Some(path) = game_log_path() else { return out };
    let Ok(file) = File::open(path) else { return out };
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    while out.glz.is_none() || out.version.is_none() {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        if out.glz.is_none() {
            out.glz = parse_glz(&line);
        }
        if out.version.is_none() {
            out.version = parse_version(&line);
        }
    }
    out
}

/// `...https://glz-eu-1.eu.a.pvp.net/...` → ("eu", "eu")
fn parse_glz(line: &str) -> Option<(String, String)> {
    let host = line.split("https://glz-").nth(1)?.split(['/', ' ', '"']).next()?;
    let mut labels = host.split('.');
    let region = labels.next()?.strip_suffix("-1")?;
    let shard = labels.next()?;
    (!region.is_empty() && !shard.is_empty()).then(|| (region.to_owned(), shard.to_owned()))
}

/// `CI server version: release-09.10-14-2899766` → `release-09.10-shipping-14-2899766`
fn parse_version(line: &str) -> Option<String> {
    let raw = line.split("CI server version: ").nth(1)?.trim();
    if raw.contains("-shipping-") {
        return Some(raw.to_owned());
    }
    let mut parts: Vec<&str> = raw.split('-').collect();
    if parts.len() < 3 {
        return None;
    }
    parts.insert(2, "shipping");
    Some(parts.join("-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lockfile_fields() {
        let lock = Lockfile::parse("Riot Client:18764:51923:Xy-9_abc:https\n").unwrap();
        assert_eq!((lock.port, lock.password.as_str()), (51923, "Xy-9_abc"));
        assert!(Lockfile::parse("Riot Client:18764:notaport:pw:https").is_none());
        assert!(Lockfile::parse("").is_none());
    }

    #[test]
    fn glz_region_and_shard_from_log() {
        let line = "[2026.11.14-10.00.00:000][  0]LogPlatformCommon: Platform HTTP Query End. QueryName: \
                    [GET_CORE_GAME], URL [GET https://glz-latam-1.na.a.pvp.net/core-game/v1/players/abc]";
        assert_eq!(parse_glz(line), Some(("latam".into(), "na".into())));
        assert_eq!(parse_glz("https://glz-eu-1.eu.a.pvp.net"), Some(("eu".into(), "eu".into())));
        assert_eq!(parse_glz("https://pd.na.a.pvp.net/mmr"), None);
    }

    #[test]
    fn client_version_gets_shipping_segment() {
        let line = "LogShooter: Display: CI server version: release-13.10-18-5893614\r\n";
        assert_eq!(parse_version(line).as_deref(), Some("release-13.10-shipping-18-5893614"));
        let already = "CI server version: release-13.10-shipping-18-5893614";
        assert_eq!(parse_version(already).as_deref(), Some("release-13.10-shipping-18-5893614"));
    }

    #[test]
    fn presence_state_old_and_new_layouts() {
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        assert_eq!(parse_loop_state(&b64(r#"{"sessionLoopState":"INGAME"}"#)), LoopState::InGame);
        assert_eq!(
            parse_loop_state(&b64(r#"{"matchPresenceData":{"sessionLoopState":"PREGAME"}}"#)),
            LoopState::Pregame
        );
        assert_eq!(parse_loop_state(&b64(r#"{"isValid":true}"#)), LoopState::Unknown);
        assert_eq!(parse_loop_state("%%not-base64%%"), LoopState::Unknown);
    }

    #[test]
    fn log_tail_sees_only_new_complete_lines() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("vltracker-tail-{}.log", std::process::id()));
        std::fs::write(&path, "Gameplay started at local time 0.01 (old match)\n").unwrap();
        let mut tail = LogTail { offset: std::fs::metadata(&path).unwrap().len(), path: path.clone() };
        let markers = vec!["Gameplay started at local time".to_owned()];
        assert!(!tail.saw_any(&markers), "history before the start offset is ignored");

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"LogFoo: loading\nGameplay started at loc").unwrap();
        assert!(!tail.saw_any(&markers), "partial line must not be consumed");
        f.write_all(b"al time 0.02 (server time 0.000000)\n").unwrap();
        assert!(tail.saw_any(&markers), "completed line is matched");

        std::fs::write(&path, "Gameplay started at local time 0.03\n").unwrap();
        assert!(tail.saw_any(&markers), "a recreated (shorter) log is read from the start");
        std::fs::remove_file(&path).unwrap();
    }
}
