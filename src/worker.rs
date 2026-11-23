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
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use eframe::egui;
use parking_lot::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until};

use crate::henrik_api::{Account, Henrik, IMPACT_PAGE};
use crate::impact::{self, MatchDto, WinModel};
use crate::match_cache::MatchCache;
use crate::model::{
    Command, Depth, Error, Event, Fetch, IconKey, Progress, Result, Roster, Status, UiSink,
};
use crate::riot_local::{Lockfile, LogTail, LoopState, RiotSession};
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
/// Safety limit for unusually long histories; the UI labels this as an upper bound.
const ACT_MATCH_CAP: u32 = 300;

pub fn spawn(
    ctx: egui::Context,
    api_key: String,
    round_start_markers: Vec<String>,
) -> (mpsc::Receiver<Event>, UnboundedSender<Command>) {
    let (event_tx, event_rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let ui = UiSink::new(event_tx, ctx);
    let spawned = std::thread::Builder::new()
        .name("vltracker-worker".into())
        .spawn({
            let ui = ui.clone();
            move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        return ui
                            .send(Event::Notice(Some(format!("Failed to start runtime: {e}"))));
                    }
                };
                match Worker::new(ui.clone(), api_key, round_start_markers) {
                    Ok(worker) => rt.block_on(worker.run(cmd_rx)),
                    Err(e) => ui.send(Event::Notice(Some(format!(
                        "Failed to start HTTP client: {e}"
                    )))),
                }
            }
        });
    if let Err(e) = spawned {
        ui.send(Event::Notice(Some(format!(
            "Failed to start worker thread: {e}"
        ))));
    }
    (event_rx, cmd_tx)
}

enum TaskDone {
    /// `ok == false` means a transient failure; the player is re-scheduled on the next tick.
    Player {
        puuid: String,
        ok: bool,
        cheap_ok: bool,
        depth: Depth,
        had_assets: bool,
    },
    Lookup {
        query: String,
        result: Result<Account>,
    },
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
    /// Shared by player jobs; learns from every match fetched and persists to disk.
    win_model: Arc<Mutex<WinModel>>,
    cache: Arc<Mutex<MatchCache>>,
    /// Requested impact depth, shared with running jobs.
    depth: Arc<AtomicU8>,
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
    lookup_query: Option<String>,
    lookup_target: Option<(String, String)>,
    fetched: HashMap<String, (Instant, Depth)>,
    cheap_fetched: HashMap<String, Instant>,
    in_flight: HashSet<String>,
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
            win_model: Arc::new(Mutex::new(WinModel::load())),
            cache: Arc::new(Mutex::new(MatchCache::load())),
            depth: Arc::new(AtomicU8::new(depth_to_u8(Depth::Last10))),
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
            lookup_query: None,
            lookup_target: None,
            fetched: HashMap::new(),
            cheap_fetched: HashMap::new(),
            in_flight: HashSet::new(),
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
                self.cheap_fetched.clear();
                self.fetched.clear();
                self.schedule_roster();
            }
            Command::Lookup(query) => self.lookup(query),
            Command::LoadIcon(key) => self.load_icon(key),
            Command::SetDepth(depth) => {
                self.depth.store(depth_to_u8(depth), Ordering::Relaxed);
                // In-flight jobs observe the shared target; never launch a second job for one
                // player or repeat cheap rank/stats requests just because the scope changed.
                self.schedule_roster();
            }
        }
    }

    fn task_done(&mut self, done: TaskDone) {
        match done {
            TaskDone::Player {
                puuid,
                ok,
                cheap_ok,
                depth,
                had_assets,
            } => {
                if cheap_ok {
                    self.cheap_fetched.insert(puuid.clone(), Instant::now());
                }
                self.in_flight.remove(&puuid);
                let changed = self.depth.load(Ordering::Relaxed) != depth_to_u8(depth);
                let needs_assets = depth == Depth::Act && !had_assets && self.assets.is_some();
                if !ok || changed || needs_assets {
                    self.fetched.remove(&puuid);
                }
                if changed || needs_assets {
                    self.schedule_roster();
                }
            }
            TaskDone::Lookup { query, result } => match result {
                Ok(account) if self.lookup_query.as_deref() == Some(&query) => {
                    self.named.insert(account.puuid.clone());
                    self.ui.send(Event::Name {
                        puuid: account.puuid.clone(),
                        riot_id: account.riot_id,
                    });
                    self.ui.send(Event::LookupResolved {
                        query,
                        puuid: account.puuid.clone(),
                    });
                    self.lookup_target = Some((account.puuid.clone(), account.region.clone()));
                    self.schedule(&account.puuid, &account.region);
                }
                Ok(_) => {}
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
            LoopState::InGame if self.roster_is_core && self.roster.is_some() => {
                Ok(self.poll_loading())
            }
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
            let fallback = self
                .assets
                .as_ref()
                .and_then(|a| a.client_version.as_deref());
            match RiotSession::connect(lock, self.local.clone(), self.remote.clone(), fallback)
                .await
            {
                Ok(s) => {
                    self.session = Some(Arc::new(s));
                    self.set_notice(NoticeSource::Riot, None);
                }
                // Not logged in / still booting surfaces as HTTP errors; stay quiet and retry.
                Err(e @ Error::Missing(_)) => {
                    self.set_notice(NoticeSource::Riot, Some(format!("Riot Client: {e}")))
                }
                Err(_) => {}
            }
        }
        self.session.clone()
    }

    /// Returns whether an agent-select lobby exists.
    async fn poll_pregame(&mut self, session: &RiotSession) -> Result<bool> {
        let Some(id) = session.pregame_match_id().await? else {
            return Ok(false);
        };
        // Re-read every tick: hovered/locked agents change during selection.
        let roster = session.pregame_roster(&id).await?;
        self.set_status(Status::AgentSelect);
        self.apply_roster(session, roster, false).await;
        self.log_tail = LogTail::game_log_at_end();
        Ok(true)
    }

    /// Returns whether a loaded match exists. Fetches the roster only for a new match ID.
    async fn poll_core(&mut self, session: &RiotSession) -> Result<bool> {
        let Some(id) = session.core_match_id().await? else {
            return Ok(false);
        };
        let known = self.roster_is_core && self.roster.as_ref().is_some_and(|r| r.match_id == id);
        if !known {
            let roster = session.core_roster(&id).await?;
            self.apply_roster(session, roster, true).await;
            // Only a match entered from agent select has a loading screen we watched start.
            self.loading = self.log_tail.take().map(|tail| (tail, Instant::now()));
        }
        self.set_status(if self.loading.is_some() {
            Status::Loading
        } else {
            Status::LiveMatch
        });
        Ok(true)
    }

    /// Ends the loading phase on the first round-start log line (or after `LOADING_MAX`).
    fn poll_loading(&mut self) -> Duration {
        let Some((tail, since)) = &mut self.loading else {
            return MATCH_POLL;
        };
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
        let names = if unnamed.is_empty() {
            Ok(Vec::new())
        } else {
            session.names(&unnamed).await
        };
        self.set_roster(Some(roster), is_core);
        // On failure, unresolved players fall back to a HenrikDev account lookup in their fetch job.
        for (puuid, riot_id) in names.unwrap_or_default() {
            self.named.insert(puuid.clone());
            self.ui.send(Event::Name { puuid, riot_id });
        }
        self.schedule_roster();
    }

    /// Queue visible roster players and the manually searched player, if any.
    fn schedule_roster(&mut self) {
        let roster = match (&self.roster, &self.session) {
            (Some(roster), Some(session)) => roster
                .slots
                .iter()
                .filter(|s| !s.hidden)
                .map(|s| (s.puuid.clone(), session.region.clone()))
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        for (puuid, region) in roster {
            self.schedule(&puuid, &region);
        }
        if let Some((puuid, region)) = self.lookup_target.clone() {
            self.schedule(&puuid, &region);
        }
    }

    fn schedule(&mut self, puuid: &str, region: &str) {
        let depth = depth_from_u8(self.depth.load(Ordering::Relaxed));
        if self.in_flight.contains(puuid) {
            return;
        }
        let cheap_fresh = self
            .cheap_fetched
            .get(puuid)
            .is_some_and(|t| t.elapsed() < PLAYER_TTL);
        if self
            .fetched
            .get(puuid)
            .is_some_and(|(t, d)| t.elapsed() < PLAYER_TTL && *d == depth)
        {
            return;
        }
        self.in_flight.insert(puuid.to_owned());
        self.fetched
            .insert(puuid.to_owned(), (Instant::now(), depth));
        let job = PlayerJob {
            session: self.session.clone(),
            henrik: self.henrik.clone(),
            win_model: self.win_model.clone(),
            cache: self.cache.clone(),
            depth: self.depth.clone(),
            assets: self.assets.clone(),
            ui: self.ui.clone(),
            puuid: puuid.to_owned(),
            region: region.to_owned(),
            need_name: !self.named.contains(puuid),
            need_cheap: !cheap_fresh,
            initial_depth: depth,
        };
        self.tasks.spawn(job.run());
    }

    fn lookup(&mut self, query: String) {
        let parsed = query
            .rsplit_once('#')
            .map(|(n, t)| (n.trim().to_owned(), t.trim().to_owned()));
        self.lookup_query = Some(query.clone());
        self.lookup_target = None;
        let Some((name, tag)) = parsed.filter(|(n, t)| !n.is_empty() && !t.is_empty()) else {
            return self.ui.send(Event::LookupFailed {
                query,
                reason: "Use the format Name#Tag".into(),
            });
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
                if depth_from_u8(self.depth.load(Ordering::Relaxed)) == Depth::Act {
                    self.fetched.clear();
                    self.schedule_roster();
                }
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
        let Some(url) = assets.icon_url(&key).map(str::to_owned) else {
            return;
        };
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
            Some(msg) => self
                .notices
                .insert(source, msg.clone())
                .is_none_or(|old| old != msg),
            None => self.notices.remove(&source).is_some(),
        };
        if changed {
            let joined = (!self.notices.is_empty()).then(|| {
                self.notices
                    .values()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join("  ·  ")
            });
            self.ui.send(Event::Notice(joined));
        }
    }
}

struct PlayerJob {
    session: Option<Arc<RiotSession>>,
    henrik: Arc<Henrik>,
    win_model: Arc<Mutex<WinModel>>,
    cache: Arc<Mutex<MatchCache>>,
    /// Map callouts for site-entry detection; entries are skipped until assets load.
    assets: Option<Arc<Assets>>,
    ui: UiSink,
    puuid: String,
    region: String,
    need_name: bool,
    need_cheap: bool,
    initial_depth: Depth,
    /// Shared target; read before every page so a change applies mid-fetch.
    depth: Arc<AtomicU8>,
}

/// Matches per depth level.
fn depth_matches(d: Depth) -> u32 {
    match d {
        Depth::Last10 => 10,
        Depth::Last20 => 20,
        Depth::Act => ACT_MATCH_CAP,
    }
}

impl PlayerJob {
    async fn run(self) -> TaskDone {
        let mut transient = false;
        let mut cheap_transient = false;

        if self.need_name {
            match self.henrik.account_by_puuid(&self.puuid).await {
                Ok(a) => self.ui.send(Event::Name {
                    puuid: self.puuid.clone(),
                    riot_id: a.riot_id,
                }),
                Err(e) => transient |= e.is_transient(),
            }
        }

        if self.need_cheap {
            // Riot PD is free and fast; HenrikDev only if it fails (e.g. format change after a patch).
            let pd = match &self.session {
                Some(s) => s.rank(&self.puuid).await,
                None => Err(Error::Missing("Riot session")),
            };
            let rank = match pd {
                Ok(rank) => Ok(rank),
                Err(_) => self.henrik.rank(&self.region, &self.puuid).await,
            };
            cheap_transient |= rank.as_ref().is_err_and(Error::is_transient);
            self.ui.send(Event::Rank {
                puuid: self.puuid.clone(),
                rank: Fetch::from(rank),
            });

            let stats = self.henrik.stats(&self.region, &self.puuid).await;
            cheap_transient |= stats.as_ref().is_err_and(Error::is_transient);
            self.ui.send(Event::Stats {
                puuid: self.puuid.clone(),
                stats: Fetch::from(stats),
            });
        }

        transient |= self.deep_stats().await;
        TaskDone::Player {
            puuid: self.puuid,
            ok: !transient && !cheap_transient,
            cheap_ok: self.need_cheap && !cheap_transient,
            depth: self.initial_depth,
            had_assets: self.assets.is_some(),
        }
    }

    /// Full-match history, newest first. Only a prefix the newest API page confirms is reused:
    /// incidental matches cached from lobby-mates may leave gaps. A rating is published after
    /// every page so the user sees something within seconds while the deeper scope loads.
    async fn deep_stats(&self) -> bool {
        let act = match &self.session {
            Some(s) => s.current_act().await.ok(),
            None => None,
        }
        .or_else(|| {
            self.assets
                .as_ref()
                .and_then(|a| a.latest_act().map(str::to_owned))
        });
        let target0 = depth_from_u8(self.depth.load(Ordering::Relaxed));
        if target0 == Depth::Act && act.is_none() {
            self.unavailable(Depth::Act, "Current act unavailable");
            return false;
        }

        let (mut matches, mut exhausted) = self.cache.lock().prefix(&self.puuid);
        // The API's newest page is the only authority on what the latest matches are.
        let mut page = match self.fetch_page(0, target0).await {
            Page::Ok(page) => page,
            Page::Failed { transient, reason } => {
                if matches.is_empty() {
                    self.unavailable(target0, &reason);
                } else {
                    // Unverified cached prefix: show it, capped at the depth it covers.
                    let depth = staged_depth(matches.len() as u32, exhausted).min(target0);
                    self.publish(&matches, depth, &act, false, exhausted);
                }
                return transient;
            }
        };
        {
            let mut cache = self.cache.lock();
            if prefix_matches(&matches, &page) {
                exhausted |= (page.len() as u32) < IMPACT_PAGE;
            } else {
                let short = (page.len() as u32) < IMPACT_PAGE;
                matches = cache.insert(page);
                exhausted = short;
            }
            cache.set_prefix(&self.puuid, &matches, exhausted);
        }
        if matches.is_empty() {
            self.unavailable(target0, "No competitive matches");
            return false;
        }
        let mut start = matches.len() as u32;
        loop {
            let target = depth_from_u8(self.depth.load(Ordering::Relaxed));
            if target == Depth::Act && act.is_none() {
                self.unavailable(Depth::Act, "Current act unavailable");
                return false;
            }
            let have = matches.len() as u32;
            let act_done = act.as_deref().is_some_and(|a| {
                matches
                    .last()
                    .is_some_and(|m| m.act().as_deref().is_some_and(|b| b != a))
            });
            let history_complete = exhausted || act_done;
            let done = history_complete || have >= depth_matches(target);
            let depth = if done {
                target
            } else {
                staged_depth(have, history_complete).min(target)
            };
            self.publish(&matches, depth, &act, done, history_complete);
            if done {
                return false;
            }
            page = match self.fetch_page(start, target).await {
                Page::Ok(page) => page,
                Page::Failed { transient, .. } => return transient,
            };
            let page_len = page.len() as u32;
            {
                let mut cache = self.cache.lock();
                matches.extend(cache.insert(page));
                exhausted = page_len < IMPACT_PAGE;
                start += page_len;
                cache.set_prefix(&self.puuid, &matches, exhausted);
            }
        }
    }

    /// One page toward `target`, waiting out the rate-limit window so cheap calls (names,
    /// ranks, lookups) keep their share of the HenrikDev budget.
    async fn fetch_page(&self, start: u32, target: Depth) -> Page {
        loop {
            let wait = self.henrik.deep_wait_secs();
            if wait > 0 {
                self.ui.send(Event::ImpactProgress {
                    puuid: self.puuid.clone(),
                    progress: Progress::RateLimited {
                        have: start,
                        target,
                        resume_secs: wait,
                    },
                });
                tokio::time::sleep(Duration::from_secs(wait.into())).await;
                continue;
            }
            self.ui.send(Event::ImpactProgress {
                puuid: self.puuid.clone(),
                progress: Progress::Fetching {
                    have: start,
                    target,
                },
            });
            return match self
                .henrik
                .recent_matches(&self.region, &self.puuid, start)
                .await
            {
                Ok(page) => {
                    self.report_quota();
                    Page::Ok(page)
                }
                Err(e) => Page::Failed {
                    transient: e.is_transient(),
                    reason: e.to_string(),
                },
            };
        }
    }

    fn unavailable(&self, depth: Depth, reason: &str) {
        self.ui.send(Event::Impact {
            puuid: self.puuid.clone(),
            depth,
            impact: Fetch::Unavailable(reason.to_owned()),
        });
        self.ui.send(Event::ImpactProgress {
            puuid: self.puuid.clone(),
            progress: Progress::Done,
        });
    }

    /// Rates the player over `matches` (trimmed to `depth`), updates the model, and publishes.
    /// `complete` marks that nothing older is known to exist for this player.
    fn publish(
        &self,
        matches: &[Arc<MatchDto>],
        depth: Depth,
        act: &Option<String>,
        done: bool,
        complete: bool,
    ) {
        let scope: Vec<Arc<MatchDto>> = match depth {
            Depth::Act => matches
                .iter()
                .filter(|m| {
                    act.as_deref()
                        .is_some_and(|a| m.act().as_deref() == Some(a))
                })
                .cloned()
                .collect(),
            d => matches
                .iter()
                .take(depth_matches(d) as usize)
                .cloned()
                .collect(),
        };
        let impact = {
            let mut model = self.win_model.lock();
            if model.ingest(matches) {
                let _ = model.save();
            }
            impact::evaluate(&scope, &self.puuid, &model, self.assets.as_deref()).map(|mut i| {
                i.history_complete = complete;
                i
            })
        };
        self.ui.send(Event::Impact {
            puuid: self.puuid.clone(),
            depth,
            impact: Fetch::from(impact.ok_or(Error::NotFound)),
        });
        if done {
            self.ui.send(Event::ImpactProgress {
                puuid: self.puuid.clone(),
                progress: Progress::Done,
            });
        }
    }

    fn report_quota(&self) {
        if let Some(q) = self.henrik.quota() {
            self.ui.send(Event::Quota {
                remaining: q.remaining,
                reset_secs: q.reset_secs,
            });
        }
    }
}

enum Page {
    Ok(Vec<MatchDto>),
    Failed { transient: bool, reason: String },
}

/// The deepest level the fetched matches can support right now; a complete history means the
/// act view has everything there is, however few matches that is.
fn staged_depth(have: u32, history_complete: bool) -> Depth {
    if history_complete {
        Depth::Act
    } else if have >= 20 {
        Depth::Last20
    } else {
        Depth::Last10
    }
}

/// Whether the API's newest page continues the cache's claimed prefix; only then is the prefix
/// trusted to be contiguous with Riot's history.
fn prefix_matches(matches: &[Arc<MatchDto>], page: &[MatchDto]) -> bool {
    !page.is_empty()
        && matches.len() >= page.len()
        && matches
            .iter()
            .zip(page)
            .all(|(old, new)| old.id() == new.id())
}

fn depth_from_u8(v: u8) -> Depth {
    match v {
        0 => Depth::Last10,
        1 => Depth::Last20,
        _ => Depth::Act,
    }
}

fn depth_to_u8(d: Depth) -> u8 {
    match d {
        Depth::Last10 => 0,
        Depth::Last20 => 1,
        Depth::Act => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dto(id: &str) -> MatchDto {
        serde_json::from_str(&format!(
            r#"{{"metadata":{{"match_id":"{id}"}},"players":[]}}"#
        ))
        .unwrap()
    }

    #[test]
    fn staging_grows_with_fetched_matches_and_completes_the_history() {
        assert_eq!(staged_depth(0, false), Depth::Last10);
        assert_eq!(staged_depth(19, false), Depth::Last10);
        assert_eq!(staged_depth(20, false), Depth::Last20);
        // Exhausted or past the act boundary: everything there is, even one match.
        assert_eq!(staged_depth(1, true), Depth::Act);
        assert_eq!(staged_depth(3, true).max(Depth::Last10), Depth::Act);
    }

    #[test]
    fn a_cached_prefix_must_agree_with_the_newest_page() {
        let page = vec![dto("a"), dto("b")];
        let same: Vec<Arc<MatchDto>> = page.iter().map(|m| Arc::new(dto(m.id()))).collect();
        assert!(prefix_matches(&same, &page));
        // A page that disagrees about what the newest matches are breaks the chain.
        let reordered: Vec<Arc<MatchDto>> =
            page.iter().rev().map(|m| Arc::new(dto(m.id()))).collect();
        assert!(!prefix_matches(&reordered, &page));
        assert!(
            !prefix_matches(&[], &page),
            "an empty cache never confirms anything"
        );
        assert!(!prefix_matches(&same, &[]), "an empty page proves nothing");
    }

    #[test]
    fn a_longer_confirmed_prefix_is_kept() {
        // Newest-first: the page must agree on the newest entries; older cached ones it has
        // not reached yet were confirmed by an earlier session and stay.
        let page = vec![dto("c"), dto("b")];
        let prefix: Vec<Arc<MatchDto>> =
            ["c", "b", "a"].iter().map(|id| Arc::new(dto(id))).collect();
        assert!(prefix_matches(&prefix, &page));
        let wrong: Vec<Arc<MatchDto>> =
            ["b", "c", "a"].iter().map(|id| Arc::new(dto(id))).collect();
        assert!(!prefix_matches(&wrong, &page));
    }

    #[test]
    fn depth_round_trips_through_the_shared_atomic() {
        for d in [Depth::Last10, Depth::Last20, Depth::Act] {
            assert_eq!(depth_from_u8(depth_to_u8(d)), d);
        }
    }
}
