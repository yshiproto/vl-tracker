//! Types shared between the background worker and the UI thread.

use std::fmt;
use std::sync::Arc;
use std::sync::mpsc;

use eframe::egui;

use crate::val_assets::Assets;

/// Coarse client state shown in the top bar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    WaitingForClient,
    InMenu,
    AgentSelect,
    /// Match loaded, loading screen still up (no round started yet).
    Loading,
    LiveMatch,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Ally,
    Enemy,
}

/// One player slot in the current lobby, as reported by Riot's GLZ servers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Slot {
    pub puuid: String,
    pub side: Side,
    /// Lowercase agent UUID; `None` before a pick is hovered.
    pub agent: Option<String>,
    pub locked: bool,
    /// Player enabled "Hide my name". We never resolve or look up hidden players.
    pub hidden: bool,
    pub is_self: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Roster {
    pub match_id: String,
    /// Deathmatch-style lobby with no two-team split.
    pub ffa: bool,
    pub slots: Vec<Slot>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rank {
    /// Competitive tier index (0 = unranked) matching valorant-api `tiers[].tier`.
    pub tier: u32,
    pub rr: u32,
}

/// Final and peak rank in one act. Tiers are in that act's own table numbering;
/// convert with `Assets::normalize_tier` before comparing or displaying.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ActRank {
    /// Lowercase act UUID.
    pub act: String,
    pub tier: u32,
    /// Highest tier a win was recorded at during the act.
    pub peak_tier: u32,
    pub wins: u32,
    pub games: u32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RankProfile {
    pub current: Rank,
    pub acts: Vec<ActRank>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Stats {
    pub kd: f32,
    pub hs_pct: f32,
    pub matches: u32,
    pub wins: u32,
    pub losses: u32,
    /// Average combat score per round.
    pub acs: f32,
    /// Average damage per round.
    pub adr: f32,
}

/// Contribution to winning rounds over a player's recent competitive matches. The rating is a
/// round-swing (win-probability-added) score in the style of HLTV Rating 3.0; the remaining
/// fields break it down by situation and role.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Impact {
    /// 1.00 = break-even; ±0.15 ≈ one standard deviation across players seen.
    pub rating: f32,
    /// Round-win probability added per round, shrunk toward 0 for small samples (0.023 = +2.3%).
    pub wpa_per_round: f32,
    pub rounds: u32,
    pub matches: u32,
    /// Most-played agent in the sample (lowercase UUID).
    pub main_agent: Option<String>,
    pub kills: u32,
    pub deaths: u32,
    pub opening_wins: u32,
    pub opening_losses: u32,
    /// Share of rounds with a kill, assist, survival, or traded death.
    pub kast: f32,
    pub clutches: u32,
    pub clutches_won: u32,
    /// Non-opening kills that swung the round by 25+ points.
    pub impact_frags: u32,
    /// Kills that moved round-win probability by under 3 points (exit frags, eco kills, …).
    pub low_impact_kills: u32,

    /// Trade kills within 2 s of the teammate's death, and within 2–5 s.
    pub trades_fast: u32,
    pub trades_late: u32,
    pub deaths_traded: u32,
    /// A nearby teammate died and their killer survived the next 5 s while this player was alive.
    pub missed_refrags: u32,

    /// Assists on players this player never damaged that round (flashes, debuffs, …).
    pub util_assists: u32,
    pub ability_kills: u32,
    pub casts_per_round: f32,
    /// Same agent's average across all matches the app has seen.
    pub agent_casts_per_round: Option<f32>,

    /// Attacking side's first fight inside a site area before the plant, won or lost by this player.
    pub entry_wins: u32,
    pub entry_losses: u32,
    /// Most common callout of entry kills, e.g. "A Main".
    pub entry_spot: Option<String>,

    /// Attack rounds where the player's own fight (or the plant) happened far from every teammate.
    pub lurk_rounds: u32,
    /// Average round swing in those rounds.
    pub lurk_swing: f32,
    /// Isolated kills after the round was already decided.
    pub lurk_late_kills: u32,
    pub lurk_deaths_untraded: u32,

    /// Most recent rounds first, from the latest two matches.
    pub recent_rounds: Vec<RoundSwing>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct RoundSwing {
    /// This player's round-win probability added in the round.
    pub swing: f32,
    pub won: bool,
    pub kills: u8,
    pub died: bool,
    pub planted: bool,
    pub defused: bool,
    /// First round of a match (draw a separator before it).
    pub match_start: bool,
}

/// Per-field load state so each cell renders independently.
#[derive(Clone, PartialEq, Debug, Default)]
pub enum Fetch<T> {
    #[default]
    Pending,
    Ready(T),
    Unavailable(String),
}

impl<T> From<Result<T, Error>> for Fetch<T> {
    fn from(r: Result<T, Error>) -> Self {
        match r {
            Ok(v) => Fetch::Ready(v),
            Err(e) => Fetch::Unavailable(e.to_string()),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum IconKey {
    Agent(String),
    Rank(u32),
}

/// Worker → UI.
pub enum Event {
    Status(Status),
    Assets(Arc<Assets>),
    Roster(Option<Roster>),
    Name { puuid: String, riot_id: String },
    Rank { puuid: String, rank: Fetch<RankProfile> },
    Stats { puuid: String, stats: Fetch<Stats> },
    Impact { puuid: String, impact: Fetch<Impact> },
    Icon { key: IconKey, image: egui::ColorImage },
    LookupResolved { query: String, puuid: String },
    LookupFailed { query: String, reason: String },
    Notice(Option<String>),
}

/// UI → worker.
pub enum Command {
    SetApiKey(String),
    Lookup(String),
    LoadIcon(IconKey),
}

/// Sends events to the UI and wakes it. The only source of repaints besides user input.
#[derive(Clone)]
pub struct UiSink {
    tx: mpsc::Sender<Event>,
    ctx: egui::Context,
}

impl UiSink {
    pub fn new(tx: mpsc::Sender<Event>, ctx: egui::Context) -> Self {
        Self { tx, ctx }
    }

    pub fn send(&self, event: Event) {
        if self.tx.send(event).is_ok() {
            self.ctx.request_repaint();
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Http(reqwest::Error),
    Status(reqwest::StatusCode),
    NotFound,
    RateLimited,
    NoApiKey,
    InvalidApiKey,
    Io(std::io::Error),
    Image(image::ImageError),
    Missing(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http(e) if e.is_decode() => write!(f, "unexpected response format"),
            Error::Http(e) if e.is_connect() => write!(f, "connection failed"),
            Error::Http(e) if e.is_timeout() => write!(f, "request timed out"),
            Error::Http(e) => write!(f, "network error: {e}"),
            Error::Status(s) => write!(f, "HTTP {}", s.as_u16()),
            Error::NotFound => write!(f, "no data"),
            Error::RateLimited => write!(f, "rate limited"),
            Error::NoApiKey => write!(f, "no HenrikDev API key"),
            Error::InvalidApiKey => write!(f, "HenrikDev API key rejected"),
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Image(e) => write!(f, "image error: {e}"),
            Error::Missing(what) => write!(f, "missing {what}"),
        }
    }
}

impl Error {
    /// Worth retrying later (network, server, quota) as opposed to a definitive answer.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Http(_) | Error::RateLimited => true,
            Error::Status(s) => s.is_server_error() || *s == reqwest::StatusCode::REQUEST_TIMEOUT,
            _ => false,
        }
    }
}

impl std::error::Error for Error {}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Http(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<image::ImageError> for Error {
    fn from(e: image::ImageError) -> Self {
        Error::Image(e)
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
