#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod henrik_api;
mod impact;
mod model;
mod overlay;
mod riot_local;
mod val_assets;
mod win32;
mod worker;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc;

use eframe::egui::{self, Color32, RichText};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::Config;
use crate::model::{Command, Event, Fetch, IconKey, Impact, RankProfile, Roster, Side, Slot, Stats, Status};
use crate::overlay::OverlayState;
use crate::val_assets::Assets;
use crate::win32::Hotkey;

const ICON_SIZE: f32 = 28.0;

fn main() -> eframe::Result {
    // reqwest is built without a bundled crypto provider; use ring for all TLS.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("vltracker")
            .with_inner_size([1040.0, 440.0])
            .with_min_inner_size([760.0, 300.0]),
        ..Default::default()
    };
    eframe::run_native("vltracker", options, Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))))
}

#[derive(Default)]
struct PlayerInfo {
    riot_id: Option<String>,
    rank: Fetch<RankProfile>,
    stats: Fetch<Stats>,
    impact: Fetch<Impact>,
}

enum Lookup {
    Pending(String),
    Resolved(String),
    Failed { query: String, reason: String },
}

struct App {
    events: mpsc::Receiver<Event>,
    commands: UnboundedSender<Command>,
    config: Config,
    key_input: String,
    lookup_input: String,
    status: Status,
    /// From the worker.
    notice: Option<String>,
    /// From the UI itself (config save, dead worker).
    local_notice: Option<String>,
    assets: Option<Arc<Assets>>,
    roster: Option<Roster>,
    players: HashMap<String, PlayerInfo>,
    lookup: Option<Lookup>,
    icons: HashMap<IconKey, egui::TextureHandle>,
    icons_requested: HashSet<IconKey>,
    hotkeys: mpsc::Receiver<Hotkey>,
    overlay: OverlayState,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        let config = Config::load();
        let (events, commands) = worker::spawn(
            ctx.clone(),
            config.henrik_api_key.clone(),
            config.overlay.round_start_markers.clone(),
        );
        let (hotkey_tx, hotkeys) = mpsc::channel();
        let repaint = ctx.clone();
        let hotkey_error = win32::spawn_hotkeys(&config.overlay.hotkey, &config.overlay.calibrate_hotkey, move |key| {
            if hotkey_tx.send(key).is_ok() {
                repaint.request_repaint();
            }
        })
        .err();
        Self {
            events,
            commands,
            key_input: config.henrik_api_key.clone(),
            config,
            lookup_input: String::new(),
            status: Status::WaitingForClient,
            notice: None,
            local_notice: hotkey_error.map(|e| format!("Overlay hotkeys: {e}")),
            assets: None,
            roster: None,
            players: HashMap::new(),
            lookup: None,
            icons: HashMap::new(),
            icons_requested: HashSet::new(),
            hotkeys,
            overlay: OverlayState::default(),
        }
    }

    fn apply(&mut self, ctx: &egui::Context, event: Event) {
        match event {
            Event::Status(s) => {
                self.status = s;
                self.overlay.on_status(s);
            }
            Event::Assets(a) => self.assets = Some(a),
            Event::Roster(r) => self.roster = r,
            Event::Name { puuid, riot_id } => self.players.entry(puuid).or_default().riot_id = Some(riot_id),
            Event::Rank { puuid, rank } => self.players.entry(puuid).or_default().rank = rank,
            Event::Stats { puuid, stats } => self.players.entry(puuid).or_default().stats = stats,
            Event::Impact { puuid, impact } => self.players.entry(puuid).or_default().impact = impact,
            Event::Icon { key, image } => {
                let tex = ctx.load_texture(format!("{key:?}"), image, egui::TextureOptions::LINEAR);
                self.icons.insert(key, tex);
            }
            Event::LookupResolved { query, puuid } => {
                if matches!(&self.lookup, Some(Lookup::Pending(q)) if *q == query) {
                    self.lookup = Some(Lookup::Resolved(puuid));
                }
            }
            Event::LookupFailed { query, reason } => {
                if matches!(&self.lookup, Some(Lookup::Pending(q)) if *q == query) {
                    self.lookup = Some(Lookup::Failed { query, reason });
                }
            }
            Event::Notice(n) => self.notice = n,
        }
    }

    fn send(&mut self, cmd: Command) {
        if self.commands.send(cmd).is_err() {
            self.local_notice = Some("Background worker stopped; restart the app.".into());
        }
    }

    /// Texture for an icon, requesting it from the worker on first use.
    fn icon(&mut self, key: IconKey) -> Option<egui::TextureId> {
        if let Some(tex) = self.icons.get(&key) {
            return Some(tex.id());
        }
        if self.icons_requested.insert(key.clone()) {
            self.send(Command::LoadIcon(key));
        }
        None
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (label, color) = match self.status {
                Status::WaitingForClient => ("Waiting for Riot Client", Color32::GRAY),
                Status::InMenu => ("In Menu", Color32::from_rgb(120, 170, 255)),
                Status::AgentSelect => ("Agent Select", Color32::from_rgb(240, 200, 80)),
                Status::Loading => ("Loading Screen", Color32::from_rgb(240, 200, 80)),
                Status::LiveMatch => ("Live Match Detected", Color32::from_rgb(90, 210, 120)),
            };
            let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().circle_filled(dot.center(), 5.0, color);
            ui.label(RichText::new(label).strong());
            ui.separator();

            ui.label("HenrikDev key");
            let key = ui.add(
                egui::TextEdit::singleline(&mut self.key_input).password(true).hint_text("HDEV-…").desired_width(200.0),
            );
            let submitted = key.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let changed = self.key_input.trim() != self.config.henrik_api_key;
            if (ui.add_enabled(changed, egui::Button::new("Save")).clicked() || submitted) && changed {
                self.save_key();
            }
            ui.separator();

            ui.label("Lookup");
            let lookup = ui.add(
                egui::TextEdit::singleline(&mut self.lookup_input).hint_text("Name#Tag").desired_width(170.0),
            );
            let submitted = lookup.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("Search").clicked() || submitted {
                let query = self.lookup_input.trim().to_owned();
                if !query.is_empty() {
                    self.lookup = Some(Lookup::Pending(query.clone()));
                    self.send(Command::Lookup(query));
                }
            }
            ui.separator();
            let hint = format!(
                "Overlay: {} toggle · {} calibrate",
                self.config.overlay.hotkey, self.config.overlay.calibrate_hotkey
            );
            ui.label(RichText::new(hint).small().weak());
        });
        for notice in [&self.local_notice, &self.notice].into_iter().flatten() {
            ui.label(RichText::new(notice).small().color(Color32::from_rgb(230, 170, 90)));
        }
    }

    fn save_key(&mut self) {
        let key = self.key_input.trim().to_owned();
        self.config.henrik_api_key = key.clone();
        self.save_config();
        self.send(Command::SetApiKey(key));
    }

    fn save_config(&mut self) {
        self.local_notice = match self.config.save() {
            Ok(()) => None,
            Err(e) => Some(format!("Could not save config: {e}")),
        };
    }

    fn lookup_panel(&mut self, ui: &mut egui::Ui) {
        let mut close = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Lookup").strong());
            if ui.small_button("×").on_hover_text("Close").clicked() {
                close = true;
            }
        });
        match &self.lookup {
            Some(Lookup::Pending(q)) => {
                ui.label(RichText::new(format!("Searching {q}…")).weak());
            }
            Some(Lookup::Failed { query, reason }) => {
                ui.label(RichText::new(format!("{query}: {reason}")).color(Color32::from_rgb(230, 120, 110)));
            }
            Some(Lookup::Resolved(puuid)) => {
                let slot = Slot {
                    puuid: puuid.clone(),
                    side: Side::Ally,
                    agent: None,
                    locked: true,
                    hidden: false,
                    is_self: false,
                };
                self.player_grid(ui, "lookup_grid", &[slot], Columns::Main);
            }
            None => {}
        }
        if close {
            self.lookup = None;
        }
    }

    fn match_view(&mut self, ui: &mut egui::Ui) {
        let Some(roster) = &self.roster else {
            let msg = match self.status {
                Status::WaitingForClient => "Start the Riot Client and log in. Matches are detected automatically.",
                _ => "No active match. The lobby appears here when agent select starts.",
            };
            ui.centered_and_justified(|ui| ui.label(RichText::new(msg).weak()));
            return;
        };

        let mut slots = roster.slots.clone();
        slots.sort_by_key(|s| !s.is_self);
        if roster.ffa {
            ui.heading("Players");
            self.player_grid(ui, "ffa_grid", &slots, Columns::Main);
            return;
        }

        let (allies, enemies): (Vec<Slot>, Vec<Slot>) = slots.into_iter().partition(|s| s.side == Side::Ally);
        let pregame = self.status == Status::AgentSelect;
        ui.columns(2, |cols| {
            cols[0].heading("My Team");
            self.player_grid(&mut cols[0], "ally_grid", &allies, Columns::Main);
            cols[1].heading("Enemy Team");
            if enemies.is_empty() && pregame {
                cols[1].label(RichText::new("Enemies are revealed once the match loads.").weak());
            } else {
                self.player_grid(&mut cols[1], "enemy_grid", &enemies, Columns::Main);
            }
        });
    }

    fn player_grid(&mut self, ui: &mut egui::Ui, id: &str, slots: &[Slot], columns: Columns) {
        self.grouped_grid(ui, id, &[("", slots)], columns);
    }

    /// One grid so columns align across groups; a non-empty title adds a heading row before its group.
    fn grouped_grid(&mut self, ui: &mut egui::Ui, id: &str, groups: &[(&str, &[Slot])], columns: Columns) {
        let headers: &[&str] = match columns {
            Columns::Main => &["", "", "Riot ID", "Rank", "Impact", "K/D", "HS%"],
            Columns::Scoreboard => &["", "", "Riot ID", "Rank", "K/D", "HS%", "WR", "ACS", "ADR"],
        };
        egui::Grid::new(id).striped(true).num_columns(headers.len()).spacing([10.0, 4.0]).show(ui, |ui| {
            for (title, slots) in groups {
                if !title.is_empty() {
                    // Title sits in the Riot ID column so the narrow icon columns keep their width.
                    ui.label("");
                    ui.label("");
                    ui.label(RichText::new(*title).heading());
                    ui.end_row();
                }
                for h in headers {
                    ui.label(RichText::new(*h).small().weak());
                }
                ui.end_row();
                for slot in *slots {
                    self.player_row(ui, slot, columns);
                    ui.end_row();
                }
            }
        });
    }

    fn player_row(&mut self, ui: &mut egui::Ui, slot: &Slot, columns: Columns) {
        let agent_tex = slot.agent.clone().and_then(|a| self.icon(IconKey::Agent(a)));
        let rank = self.players.get(&slot.puuid).map(|p| p.rank.clone()).unwrap_or_default();
        let rank_tex = match &rank {
            Fetch::Ready(r) => self.icon(IconKey::Rank(r.current.tier)),
            _ => None,
        };

        // Hovered-but-not-locked agents render dimmed during agent select.
        let tint = if slot.locked { Color32::WHITE } else { Color32::from_white_alpha(90) };
        icon_cell(ui, agent_tex, tint);
        icon_cell(ui, rank_tex, Color32::WHITE);

        if slot.hidden {
            let agent = slot.agent.as_deref().and_then(|a| self.assets.as_ref()?.agent_name(a));
            let text = match agent {
                Some(name) => format!("Hidden ({name})"),
                None => "Hidden".to_owned(),
            };
            ui.label(RichText::new(text).italics().weak())
                .on_hover_text("This player hid their name; their profile is not looked up.");
            let blanks = match columns {
                Columns::Main => 4,
                Columns::Scoreboard => 6,
            };
            for _ in 0..blanks {
                ui.label(RichText::new("—").weak());
            }
            return;
        }

        let info = self.players.get(&slot.puuid);
        let name = info.and_then(|p| p.riot_id.as_deref()).unwrap_or("…");
        let name = if slot.is_self { RichText::new(name).strong().color(Color32::from_rgb(240, 200, 80)) } else { RichText::new(name) };
        ui.label(name);

        fetch_cell(ui, &rank, |_| self.rank_text(&rank));
        if columns == Columns::Main {
            let impact = info.map(|p| p.impact.clone()).unwrap_or_default();
            impact_cell(ui, &impact);
        }
        let stats = info.map(|p| p.stats.clone()).unwrap_or_default();
        let hover = match &stats {
            Fetch::Ready(s) => Some(format!("Last {} competitive matches", s.matches)),
            _ => None,
        };
        let mut cells = vec![
            fetch_cell(ui, &stats, |s| format!("{:.2}", s.kd)),
            fetch_cell(ui, &stats, |s| format!("{:.0}%", s.hs_pct)),
        ];
        if columns == Columns::Scoreboard {
            cells.push(fetch_cell(ui, &stats, |s| {
                format!("{:.0}%", s.wins as f32 * 100.0 / (s.wins + s.losses).max(1) as f32)
            }));
            cells.push(fetch_cell(ui, &stats, |s| format!("{:.0}", s.acs)));
            cells.push(fetch_cell(ui, &stats, |s| format!("{:.0}", s.adr)));
        }
        if let Some(h) = hover {
            for cell in cells {
                cell.on_hover_text(&h);
            }
        }
    }
}

fn icon_cell(ui: &mut egui::Ui, tex: Option<egui::TextureId>, tint: Color32) {
    let size = egui::vec2(ICON_SIZE, ICON_SIZE);
    match tex {
        Some(id) => ui.add(egui::Image::new(egui::load::SizedTexture::new(id, size)).tint(tint)),
        None => ui.allocate_exact_size(size, egui::Sense::hover()).1,
    };
}

/// Which stat columns a player grid shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Columns {
    /// Main window: includes the impact rating.
    Main,
    /// Overlay scoreboard: adds win rate, ACS and ADR.
    Scoreboard,
}

/// Label and colour for a 1.00-centred impact rating.
fn impact_label(rating: f32) -> (&'static str, Color32) {
    match rating {
        r if r >= 1.20 => ("Carry", Color32::from_rgb(90, 210, 120)),
        r if r >= 1.07 => ("Strong", Color32::from_rgb(150, 210, 110)),
        r if r > 0.93 => ("Neutral", Color32::from_gray(200)),
        r if r > 0.80 => ("Weak", Color32::from_rgb(230, 170, 90)),
        _ => ("Liability", Color32::from_rgb(230, 110, 100)),
    }
}

fn impact_cell(ui: &mut egui::Ui, impact: &Fetch<Impact>) {
    let Fetch::Ready(i) = impact else {
        fetch_cell(ui, impact, |_| String::new());
        return;
    };
    let (label, color) = impact_label(i.rating);
    ui.label(RichText::new(format!("{:.2}", i.rating)).color(color).strong()).on_hover_text(format!(
        "{label}: {:+.1}% round-win probability per round over {} rounds (last {} competitive matches)",
        i.wpa_per_round * 100.0,
        i.rounds,
        i.matches
    ));
}

fn fetch_cell<T>(ui: &mut egui::Ui, value: &Fetch<T>, show: impl FnOnce(&T) -> String) -> egui::Response {
    match value {
        Fetch::Pending => ui.label(RichText::new("…").weak()),
        Fetch::Ready(v) => ui.label(show(v)),
        Fetch::Unavailable(reason) => ui.label(RichText::new("—").weak()).on_hover_text(reason),
    }
}

impl eframe::App for App {
    /// Runs before each frame and whenever the worker requests a repaint, even while minimized.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(event) = self.events.try_recv() {
            self.apply(ctx, event);
        }
        while let Ok(key) = self.hotkeys.try_recv() {
            self.on_hotkey(key);
        }
    }

    /// Transparent so the overlay window shows the game through it; the main window paints
    /// opaque panels over the whole surface anyway.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(2.0);
        });
        if self.lookup.is_some() {
            egui::Panel::bottom("lookup").show(ui, |ui| {
                ui.add_space(4.0);
                self.lookup_panel(ui);
                ui.add_space(4.0);
            });
        }
        egui::CentralPanel::default().show(ui, |ui| self.match_view(ui));
        self.show_overlay(ui.ctx());
    }
}
