//! Background worker: one OS thread running a single-threaded tokio runtime.
//!
//! Polling cadence:
//! * no Riot Client: check the lockfile every 5s
//! * menus / agent select: local presence every 4s (GLZ only while in agent select)
//! * in a loaded match: local presence every 15s, no remote calls until it ends
//!
//! The UI is only woken when something it displays changes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use eframe::egui;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until};

use crate::henrik_api::{Account, Henrik};
use crate::model::{Command, Error, Event, Fetch, IconKey, Result, Roster, Status, UiSink};
use crate::riot_local::{LogTail, Lockfile, LoopState, RiotSession};
use crate::val_assets::{self, Assets};

const IDLE_POLL: Duration = Duration::from_secs(5);
const MENU_POLL: Duration = Duration::from_secs(4);
const MATCH_POLL: Duration = Duration::from_secs(15);
/// Log check cadence while the loading screen is up.
const LOADING_POLL: Duration = Duration::from_secs(1);
/// Falls back to "live" if no round-start marker shows up (e.g. the log format changed).
const LOADING_MAX: Duration = Duration::from_secs(120);
const ASSET_RETRY: Duration = Duration::from_secs(60);
/// Rank/stats are not re-requested for a player seen within this window.
const PLAYER_TTL: Duration = Duration::from_secs(30 * 60);

pub fn spawn(
    ctx: egui::Context,
    api_key: String,
    round_start_markers: Vec<String>,
) -> (mpsc::Receiver<Event>, UnboundedSender<Command>) {
    let (event_tx, event_rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let ui = UiSink::new(event_tx, ctx);
    let spawned = std::thread::Builder::new().name("vltracker-worker".into()).spawn({
        let ui = ui.clone();
        move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => return ui.send(Event::Notice(Some(format!("Failed to start runtime: {e}")))),
            };
            match Worker::new(ui.clone(), api_key, round_start_markers) {
                Ok(worker) => rt.block_on(worker.run(cmd_rx)),
                Err(e) => ui.send(Event::Notice(Some(format!("Failed to start HTTP client: {e}")))),
            }
        }
    });
    if let Err(e) = spawned {
        ui.send(Event::Notice(Some(format!("Failed to start worker thread: {e}"))));
    }
    (event_rx, cmd_tx)
}

enum TaskDone {
    /// `ok == false` means a transient failure; the player is re-scheduled on the next tick.
    Player { puuid: String, ok: bool },
    Lookup { query: String, result: Result<Account> },
}

/// Notices from independent sources are tracked separately so one clearing doesn't hide another.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NoticeSource {
    Riot,
    Assets,
}

struct Worker {
    ui: UiSink,
    /// Loopback only: accepts the Riot Client's self-signed certificate.
    local: reqwest::Client,
    /// Everything remote, with normal certificate verification.
    remote: reqwest::Client,
    henrik: Arc<Henrik>,
    assets: Option<Arc<Assets>>,
    assets_retry_at: Instant,
    icons_waiting: Vec<IconKey>,
    lockfile: Option<Lockfile>,
    session: Option<Arc<RiotSession>>,
    status: Option<Status>,
    notices: BTreeMap<NoticeSource, String>,
    roster: Option<Roster>,
    roster_is_core: bool,
    named: HashSet<String>,
    fetched: HashMap<String, Instant>,
    tasks: JoinSet<TaskDone>,
    round_start_markers: Vec<String>,
    /// Armed during agent select at the log's end, so the loading screen's lines can be followed.
    log_tail: Option<LogTail>,
    /// Set while the loading screen is up.
    loading: Option<(LogTail, Instant)>,
}

impl Worker {
    fn new(ui: UiSink, api_key: String, round_start_markers: Vec<String>) -> reqwest::Result<Self> {
        let local = reqwest::Client::builder()
            .tls_danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(5))
            .build()?;
        let remote = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("vltracker/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            ui,
            henrik: Arc::new(Henrik::new(remote.clone(), api_key)),
            local,
            remote,
            assets: None,
            assets_retry_at: Instant::now(),
            icons_waiting: Vec::new(),
            lockfile: None,
            session: None,
            status: None,
            notices: BTreeMap::new(),
            roster: None,
            roster_is_core: false,
            named: HashSet::new(),
            fetched: HashMap::new(),
            tasks: JoinSet::new(),
            round_start_markers,
            log_tail: None,
            loading: None,
        })
    }

    async fn run(mut self, mut commands: UnboundedReceiver<Command>) {
        let mut next_tick = Instant::now();
        loop {
            tokio::select! {
                cmd = commands.recv() => match cmd {
                    Some(cmd) => self.handle(cmd),
                    None => return, // UI closed
                },
                Some(done) = self.tasks.join_next(), if !self.tasks.is_empty() => {
                    if let Ok(done) = done {
                        self.task_done(done);
                    }
                }
                _ = sleep_until(next_tick) => {
                    let delay = self.tick().await;
                    next_tick = Instant::now() + delay;
                }
            }
        }
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::SetApiKey(key) => {
                self.henrik.set_key(key);
                // Re-fetch rows that failed for lack of a key.
                self.fetched.clear();
                self.schedule_roster();
            }
            Command::Lookup(query) => self.lookup(query),
            Command::LoadIcon(key) => self.load_icon(key),
        }
    }

    fn task_done(&mut self, done: TaskDone) {
        match done {
            TaskDone::Player { puuid, ok } => {
                if !ok {
                    self.fetched.remove(&puuid);
                }
            }
            TaskDone::Lookup { query, result } => match result {
                Ok(account) => {
                    self.named.insert(account.puuid.clone());
                    self.ui.send(Event::Name { puuid: account.puuid.clone(), riot_id: account.riot_id });
                    self.ui.send(Event::LookupResolved { query, puuid: account.puuid.clone() });
                    self.schedule(&account.puuid, &account.region);
                }
                Err(e) => {
                    let reason = match e {
                        Error::NotFound => "Player not found".to_owned(),
                        e => e.to_string(),
                    };
                    self.ui.send(Event::LookupFailed { query, reason });
                }
            },
        }
    }

    async fn tick(&mut self) -> Duration {
        if self.assets.is_none() && Instant::now() >= self.assets_retry_at {
            self.load_assets().await;
        }

        let Some(session) = self.ensure_session().await else {
            self.set_status(Status::WaitingForClient);
            self.set_roster(None, false);
            return IDLE_POLL;
        };

        let state = match session.loop_state().await {
            Ok(state) => state,
            Err(_) => {
                // Local API went away: client closed or restarting.
                self.session = None;
                self.set_status(Status::WaitingForClient);
                self.set_roster(None, false);
                return IDLE_POLL;
            }
        };

        let result = match state {
            LoopState::Menus => {
                self.set_status(Status::InMenu);
                self.set_roster(None, false);
                self.log_tail = None;
                Ok(MENU_POLL)
            }
            LoopState::Pregame => match self.poll_pregame(&session).await {
                // Presence and GLZ briefly disagree around phase transitions.
                Ok(false) => {
                    self.set_roster(None, false);
                    Ok(MENU_POLL)
                }
                r => r.map(|_| MENU_POLL),
            },
            // Roster loaded: presence leaving INGAME is what ends the match, no remote calls
            // beyond following the log while the loading screen is up.
            LoopState::InGame if self.roster_is_core && self.roster.is_some() => Ok(self.poll_loading()),
            LoopState::InGame => match self.poll_core(&session).await {
                Ok(false) => {
                    self.set_roster(None, false);
                    Ok(MENU_POLL)
                }
                r => r.map(|_| self.poll_loading()),
            },
            LoopState::Unknown => self.poll_unknown(&session).await,
        };
        match result {
            Ok(delay) => {
                self.set_notice(NoticeSource::Riot, None);
                self.schedule_roster(); // retries transient failures; no-op otherwise
                delay
            }
            Err(e) => {
                self.set_notice(NoticeSource::Riot, Some(format!("Riot servers: {e}")));
                MENU_POLL
            }
        }
    }

    async fn ensure_session(&mut self) -> Option<Arc<RiotSession>> {
        let lock = Lockfile::read();
        if lock != self.lockfile {
            self.session = None;
            self.lockfile = lock;
        }
        if self.session.is_none() {
            let lock = self.lockfile.as_ref()?;
            let fallback = self.assets.as_ref().and_then(|a| a.client_version.as_deref());
            match RiotSession::connect(lock, self.local.clone(), self.remote.clone(), fallback).await {
                Ok(s) => {
                    self.session = Some(Arc::new(s));
                    self.set_notice(NoticeSource::Riot, None);
                }
                // Not logged in / still booting surfaces as HTTP errors; stay quiet and retry.
                Err(e @ Error::Missing(_)) => self.set_notice(NoticeSource::Riot, Some(format!("Riot Client: {e}"))),
                Err(_) => {}
            }
        }
        self.session.clone()
    }

    /// Returns whether an agent-select lobby exists.
    async fn poll_pregame(&mut self, session: &RiotSession) -> Result<bool> {
        let Some(id) = session.pregame_match_id().await? else { return Ok(false) };
        // Re-read every tick: hovered/locked agents change during selection.
        let roster = session.pregame_roster(&id).await?;
        self.set_status(Status::AgentSelect);
        self.apply_roster(session, roster, false).await;
        self.log_tail = LogTail::game_log_at_end();
        Ok(true)
    }

    /// Returns whether a loaded match exists. Fetches the roster only for a new match ID.
    async fn poll_core(&mut self, session: &RiotSession) -> Result<bool> {
        let Some(id) = session.core_match_id().await? else { return Ok(false) };
        let known = self.roster_is_core && self.roster.as_ref().is_some_and(|r| r.match_id == id);
        if !known {
            let roster = session.core_roster(&id).await?;
            self.apply_roster(session, roster, true).await;
            // Only a match entered from agent select has a loading screen we watched start.
            self.loading = self.log_tail.take().map(|tail| (tail, Instant::now()));
        }
        self.set_status(if self.loading.is_some() { Status::Loading } else { Status::LiveMatch });
        Ok(true)
    }

    /// Ends the loading phase on the first round-start log line (or after `LOADING_MAX`).
    fn poll_loading(&mut self) -> Duration {
        let Some((tail, since)) = &mut self.loading else { return MATCH_POLL };
        if !tail.saw_any(&self.round_start_markers) && since.elapsed() < LOADING_MAX {
            return LOADING_POLL;
        }
        self.loading = None;
        self.set_status(Status::LiveMatch);
        MATCH_POLL
    }

    /// Presence was unreadable: ask GLZ directly, the way the original plan polled.
    async fn poll_unknown(&mut self, session: &RiotSession) -> Result<Duration> {
        if self.poll_core(session).await? {
            return Ok(self.poll_loading());
        }
        if self.poll_pregame(session).await? {
            return Ok(MENU_POLL);
        }
        self.set_status(Status::InMenu);
        self.set_roster(None, false);
        Ok(MENU_POLL)
    }

    async fn apply_roster(&mut self, session: &RiotSession, roster: Roster, is_core: bool) {
        if self.roster.as_ref() == Some(&roster) && self.roster_is_core == is_core {
            return;
        }
        let unnamed: Vec<&str> = roster
            .slots
            .iter()
            .filter(|s| !s.hidden && !self.named.contains(&s.puuid))
            .map(|s| s.puuid.as_str())
            .collect();
        let names = if unnamed.is_empty() { Ok(Vec::new()) } else { session.names(&unnamed).await };
        self.set_roster(Some(roster), is_core);
        // On failure, unresolved players fall back to a HenrikDev account lookup in their fetch job.
        for (puuid, riot_id) in names.unwrap_or_default() {
            self.named.insert(puuid.clone());
            self.ui.send(Event::Name { puuid, riot_id });
        }
        self.schedule_roster();
    }

    /// Queue rank/stats fetches for every visible player in the current roster.
    fn schedule_roster(&mut self) {
        let (Some(roster), Some(session)) = (&self.roster, &self.session) else { return };
        let region = session.region.clone();
        let puuids: Vec<String> = roster.slots.iter().filter(|s| !s.hidden).map(|s| s.puuid.clone()).collect();
        for puuid in puuids {
            self.schedule(&puuid, &region);
        }
    }

    fn schedule(&mut self, puuid: &str, region: &str) {
        if self.fetched.get(puuid).is_some_and(|t| t.elapsed() < PLAYER_TTL) {
            return;
        }
        self.fetched.insert(puuid.to_owned(), Instant::now());
        let job = PlayerJob {
            session: self.session.clone(),
            henrik: self.henrik.clone(),
            ui: self.ui.clone(),
            puuid: puuid.to_owned(),
            region: region.to_owned(),
            need_name: !self.named.contains(puuid),
        };
        self.tasks.spawn(job.run());
    }

    fn lookup(&mut self, query: String) {
        let parsed = query.rsplit_once('#').map(|(n, t)| (n.trim().to_owned(), t.trim().to_owned()));
        let Some((name, tag)) = parsed.filter(|(n, t)| !n.is_empty() && !t.is_empty()) else {
            return self.ui.send(Event::LookupFailed { query, reason: "Use the format Name#Tag".into() });
        };
        let henrik = self.henrik.clone();
        self.tasks.spawn(async move {
            let result = henrik.account(&name, &tag).await;
            TaskDone::Lookup { query, result }
        });
    }

    async fn load_assets(&mut self) {
        match Assets::fetch(&self.remote).await {
            Ok(assets) => {
                let assets = Arc::new(assets);
                self.ui.send(Event::Assets(assets.clone()));
                self.assets = Some(assets);
                for key in std::mem::take(&mut self.icons_waiting) {
                    self.load_icon(key);
                }
            }
            Err(e) => {
                self.assets_retry_at = Instant::now() + ASSET_RETRY;
                self.set_notice(NoticeSource::Assets, Some(format!("valorant-api.com: {e}")));
            }
        }
    }

    fn load_icon(&mut self, key: IconKey) {
        let Some(assets) = &self.assets else {
            return self.icons_waiting.push(key);
        };
        let Some(url) = assets.icon_url(&key).map(str::to_owned) else { return };
        let (http, ui) = (self.remote.clone(), self.ui.clone());
        // Fire-and-forget: a failed icon just renders as an empty slot.
        tokio::spawn(async move {
            if let Ok(image) = val_assets::load_icon(&http, &url).await {
                ui.send(Event::Icon { key, image });
            }
        });
    }

    fn set_status(&mut self, status: Status) {
        if self.status != Some(status) {
            self.status = Some(status);
            self.ui.send(Event::Status(status));
        }
    }

    fn set_roster(&mut self, roster: Option<Roster>, is_core: bool) {
        self.roster_is_core = is_core;
        if roster.is_none() {
            self.loading = None;
        }
        if self.roster != roster {
            self.roster = roster.clone();
            self.ui.send(Event::Roster(roster));
        }
    }

    fn set_notice(&mut self, source: NoticeSource, notice: Option<String>) {
        let changed = match notice {
            Some(msg) => self.notices.insert(source, msg.clone()).is_none_or(|old| old != msg),
            None => self.notices.remove(&source).is_some(),
        };
        if changed {
            let joined = (!self.notices.is_empty())
                .then(|| self.notices.values().map(String::as_str).collect::<Vec<_>>().join("  ·  "));
            self.ui.send(Event::Notice(joined));
        }
    }
}

struct PlayerJob {
    session: Option<Arc<RiotSession>>,
    henrik: Arc<Henrik>,
    ui: UiSink,
    puuid: String,
    region: String,
    need_name: bool,
}

impl PlayerJob {
    async fn run(self) -> TaskDone {
        let mut transient = false;

        if self.need_name {
            match self.henrik.account_by_puuid(&self.puuid).await {
                Ok(a) => self.ui.send(Event::Name { puuid: self.puuid.clone(), riot_id: a.riot_id }),
                Err(e) => transient |= e.is_transient(),
            }
        }

        // Riot PD is free and fast; HenrikDev only if it fails (e.g. format change after a patch).
        let pd = match &self.session {
            Some(s) => s.rank(&self.puuid).await,
            None => Err(Error::Missing("Riot session")),
        };
        let rank = match pd {
            Ok(rank) => Ok(rank),
            Err(_) => self.henrik.rank(&self.region, &self.puuid).await,
        };
        transient |= rank.as_ref().is_err_and(Error::is_transient);
        self.ui.send(Event::Rank { puuid: self.puuid.clone(), rank: Fetch::from(rank) });

        let stats = self.henrik.stats(&self.region, &self.puuid).await;
        transient |= stats.as_ref().is_err_and(Error::is_transient);
        self.ui.send(Event::Stats { puuid: self.puuid.clone(), stats: Fetch::from(stats) });

        TaskDone::Player { puuid: self.puuid, ok: !transient }
    }
}
