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
