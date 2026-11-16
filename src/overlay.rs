//! In-game overlay: a borderless, transparent, always-on-top window covering the primary monitor.
//! Nothing is injected into the game; Valorant must run in Windowed Fullscreen for it to show.
//!
//! * Agent select: a stat box beside each teammate row; click one for a detail card.
//! * Loading screen: a stat box beside all ten player cards (click-through).
//! * Live match: hidden until the hotkey shows a centred scoreboard (click-through).
//!
//! The window is clipped (`SetWindowRgn`) to the boxes it draws, so everything else on screen
//! stays clickable in the game, and it never takes focus.

use eframe::egui::{self, Align2, Color32, Id, Pos2, Rect, RichText, Sense, Vec2, ViewportId};

use crate::config::OverlayConfig;
use crate::model::{Fetch, IconKey, RankProfile, Side, Slot, Status};
use crate::win32::{self, Hotkey};
use crate::{App, PlayerInfo};

const BOX_SIZE: Vec2 = Vec2::new(250.0, 58.0);
const CARD_WIDTH: f32 = 300.0;
const BADGE: f32 = 24.0;
const INTERACTIVE_TITLE: &str = "vltracker overlay (interactive)";
const PASSTHROUGH_TITLE: &str = "vltracker overlay";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    AgentSelect,
    Loading,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Calibrate(Layout),
    AgentSelect,
    Loading,
    Scoreboard,
}

#[derive(Default)]
pub struct OverlayState {
    visible: bool,
    calibrating: Option<Layout>,
    /// Teammate whose detail card is open in agent select.
    expanded: Option<String>,
    /// Native window of the current overlay viewport, once found and styled.
    window: Option<(ViewportId, isize)>,
    /// Last hit region applied, in client pixels.
    region: Option<Vec<[i32; 4]>>,
}

impl OverlayState {
    /// Agent select and the loading screen show the overlay; the live match starts hidden.
    pub fn on_status(&mut self, status: Status) {
        self.visible = matches!(status, Status::AgentSelect | Status::Loading);
        if status != Status::AgentSelect {
            self.expanded = None;
        }
    }
}

fn panel_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(Color32::from_black_alpha(215))
        .stroke(egui::Stroke::new(1.0, Color32::from_white_alpha(28)))
        .corner_radius(6.0)
        .inner_margin(8.0)
}

impl App {
    pub(crate) fn on_hotkey(&mut self, key: Hotkey) {
        match key {
            Hotkey::Toggle => {
                if self.overlay.calibrating.is_none() && self.roster.is_some() {
                    self.overlay.visible = !self.overlay.visible;
                }
            }
            Hotkey::Calibrate => self.toggle_calibration(),
        }
    }

    fn toggle_calibration(&mut self) {
        if self.overlay.calibrating.take().is_some() {
            self.save_config();
        } else {
            let layout = if self.status == Status::Loading { Layout::Loading } else { Layout::AgentSelect };
            self.overlay.calibrating = Some(layout);
        }
    }

    fn overlay_mode(&self) -> Option<Mode> {
        if let Some(layout) = self.overlay.calibrating {
            return Some(Mode::Calibrate(layout));
        }
        if !self.overlay.visible || self.roster.is_none() {
            return None;
        }
        match self.status {
            Status::AgentSelect => Some(Mode::AgentSelect),
            Status::Loading => Some(Mode::Loading),
            Status::LiveMatch => Some(Mode::Scoreboard),
            Status::WaitingForClient | Status::InMenu => None,
        }
    }

    /// Called every frame from the main window; creates, updates, or drops the overlay window.
    pub(crate) fn show_overlay(&mut self, ctx: &egui::Context) {
        let Some(mode) = self.overlay_mode() else {
            self.overlay.window = None;
            return;
        };
        let interactive = matches!(mode, Mode::Calibrate(_) | Mode::AgentSelect);
        let title = if interactive { INTERACTIVE_TITLE } else { PASSTHROUGH_TITLE };
        // Separate viewports per input mode: winit rewrites the window's extended style whenever its
        // flags change, so the click-through setting is fixed at creation instead of toggled.
        let id = ViewportId::from_hash_of(title);

        let ppp = ctx.input(|i| i.viewport().native_pixels_per_point).unwrap_or(1.0);
        let screen = win32::primary_monitor_px()
            .map(|[w, h]| Vec2::new(w, h) / ppp)
            .or_else(|| ctx.input(|i| i.viewport().monitor_size))
            .unwrap_or(Vec2::new(1920.0, 1080.0));

        let builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            .with_taskbar(false)
            .with_active(false)
            .with_window_level(egui::WindowLevel::AlwaysOnTop)
            .with_position(Pos2::ZERO)
            .with_inner_size(screen)
            .with_mouse_passthrough(!interactive);

        ctx.show_viewport_immediate(id, builder, |ui, _| {
            self.style_overlay_window(ui.ctx(), id, title, interactive);
            let rects = match mode {
                Mode::Calibrate(layout) => {
                    self.draw_calibration(ui.ctx(), layout, screen);
                    None
                }
                Mode::AgentSelect => Some(self.draw_agent_select(ui.ctx(), screen)),
                Mode::Loading => Some(self.draw_loading(ui.ctx(), screen)),
                Mode::Scoreboard => Some(self.draw_scoreboard(ui.ctx())),
            };
            self.apply_region(ui.ctx(), rects);
        });
    }

    fn style_overlay_window(&mut self, ctx: &egui::Context, id: ViewportId, title: &str, interactive: bool) {
        if self.overlay.window.is_some_and(|(v, _)| v == id) {
            return;
        }
        match win32::find_window(title) {
            Some(hwnd) => {
                if interactive {
                    win32::set_no_activate(hwnd);
                }
                self.overlay.window = Some((id, hwnd));
                self.overlay.region = None;
            }
            // The native window appears a frame after the viewport is requested.
            None if cfg!(windows) => ctx.request_repaint(),
            None => {}
        }
    }

    /// Clips the window to `rects` (points) so the rest of the screen stays the game's.
    fn apply_region(&mut self, ctx: &egui::Context, rects: Option<Vec<Rect>>) {
        let Some((_, hwnd)) = self.overlay.window else { return };
        let ppp = ctx.pixels_per_point();
        let px = rects.map(|rects| {
            rects
                .iter()
                .map(|r| {
                    let r = r.expand(2.0);
                    [(r.min.x * ppp) as i32, (r.min.y * ppp) as i32, (r.max.x * ppp).ceil() as i32, (r.max.y * ppp).ceil() as i32]
                })
                .collect::<Vec<_>>()
        });
        if self.overlay.region != px {
            win32::set_region(hwnd, px.as_deref());
            self.overlay.region = px;
        }
    }

    fn draw_agent_select(&mut self, ctx: &egui::Context, screen: Vec2) -> Vec<Rect> {
        let allies = self.ordered_slots(Side::Ally);
        let anchors = self.config.overlay.agent_select.clone();
        let mut rects = Vec::new();
        for (i, (slot, anchor)) in allies.iter().zip(&anchors).enumerate() {
            let pos = to_screen(*anchor, screen);
            let resp = egui::Area::new(Id::unique(("agent_select_box", i)))
                .current_pos(pos)
                .sense(Sense::click())
                .order(egui::Order::Foreground)
                .show(ctx, |ui| panel_frame().show(ui, |ui| self.stat_box(ui, slot)));
            let response = resp.response.on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.clicked() && !slot.hidden {
                let open = self.overlay.expanded.as_deref() == Some(slot.puuid.as_str());
                self.overlay.expanded = (!open).then(|| slot.puuid.clone());
            }
            rects.push(response.rect);

            if self.overlay.expanded.as_deref() == Some(slot.puuid.as_str()) {
                let card_pos = pos + Vec2::new(response.rect.width() + 8.0, 0.0);
                let card = egui::Area::new(Id::unique("detail_card"))
                    .current_pos(card_pos)
                    .order(egui::Order::Foreground)
                    .show(ctx, |ui| panel_frame().show(ui, |ui| self.detail_card(ui, slot)));
                rects.push(card.response.rect);
            }
        }
        rects
    }

    fn draw_loading(&mut self, ctx: &egui::Context, screen: Vec2) -> Vec<Rect> {
        let layout = &self.config.overlay;
        let groups = [
            (Side::Ally, layout.loading_ally.clone()),
            (Side::Enemy, layout.loading_enemy.clone()),
        ];
        let mut rects = Vec::new();
        for (side, anchors) in groups {
            for (i, (slot, anchor)) in self.ordered_slots(side).iter().zip(&anchors).enumerate() {
                let resp = egui::Area::new(Id::unique(("loading_box", side == Side::Ally, i)))
                    .current_pos(to_screen(*anchor, screen))
                    .interactable(false)
                    .order(egui::Order::Foreground)
                    .show(ctx, |ui| panel_frame().show(ui, |ui| self.stat_box(ui, slot)));
                rects.push(resp.response.rect);
            }
        }
        rects
    }

    fn draw_scoreboard(&mut self, ctx: &egui::Context) -> Vec<Rect> {
        let resp = egui::Area::new(Id::unique("scoreboard"))
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .interactable(false)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                panel_frame().inner_margin(14.0).show(ui, |ui| {
                    let ffa = self.roster.as_ref().is_some_and(|r| r.ffa);
                    let (allies, enemies) = (self.ordered_slots(Side::Ally), self.ordered_slots(Side::Enemy));
                    let groups: &[(&str, &[Slot])] = if ffa {
                        &[("Players", &allies)]
                    } else {
                        &[("My Team", &allies), ("Enemy Team", &enemies)]
                    };
                    self.grouped_grid(ui, "scoreboard_grid", groups, true);
                });
            });
        vec![resp.response.rect]
    }

    fn draw_calibration(&mut self, ctx: &egui::Context, layout: Layout, screen: Vec2) {
        egui::Area::new(Id::unique("calibration_bar"))
            .anchor(Align2::CENTER_TOP, Vec2::new(0.0, 40.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                panel_frame().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Overlay calibration").strong());
                        ui.label(RichText::new("drag each box beside its player").weak());
                        ui.separator();
                        if ui.selectable_label(layout == Layout::AgentSelect, "Agent select").clicked() {
                            self.overlay.calibrating = Some(Layout::AgentSelect);
                        }
                        if ui.selectable_label(layout == Layout::Loading, "Loading screen").clicked() {
                            self.overlay.calibrating = Some(Layout::Loading);
                        }
                        ui.separator();
                        if ui.button("Reset").clicked() {
                            let defaults = OverlayConfig::default();
                            let cfg = &mut self.config.overlay;
                            match layout {
                                Layout::AgentSelect => cfg.agent_select = defaults.agent_select,
                                Layout::Loading => {
                                    cfg.loading_ally = defaults.loading_ally;
                                    cfg.loading_enemy = defaults.loading_enemy;
                                }
                            }
                        }
                        let done = format!("Save ({})", self.config.overlay.calibrate_hotkey);
                        if ui.button(done).clicked() {
                            self.toggle_calibration();
                        }
                    });
                });
            });

        let cfg = &mut self.config.overlay;
        let groups: Vec<(&str, &mut Vec<[f32; 2]>)> = match layout {
            Layout::AgentSelect => vec![("Teammate", &mut cfg.agent_select)],
            Layout::Loading => vec![("Ally", &mut cfg.loading_ally), ("Enemy", &mut cfg.loading_enemy)],
        };
        for (label, anchors) in groups {
            for (i, anchor) in anchors.iter_mut().enumerate() {
                let resp = egui::Area::new(Id::unique(("calibrate_box", label, i)))
                    .current_pos(to_screen(*anchor, screen))
                    .sense(Sense::drag())
                    .order(egui::Order::Foreground)
                    .show(ctx, |ui| {
                        panel_frame().stroke(egui::Stroke::new(1.5, Color32::from_rgb(240, 200, 80))).show(ui, |ui| {
                            ui.set_min_size(BOX_SIZE - Vec2::splat(16.0));
                            ui.label(RichText::new(format!("{label} {}", i + 1)).strong());
                            ui.label(RichText::new("drag to position").weak());
                        })
                    });
                let delta = resp.response.drag_delta();
                if delta != Vec2::ZERO {
                    let pos = to_screen(*anchor, screen) + delta;
                    *anchor = [(pos.x / screen.x).clamp(0.0, 1.0), (pos.y / screen.y).clamp(0.0, 1.0)];
                }
            }
        }
    }

    /// Compact per-player box: rank badge, name, rank, and headline stats.
    fn stat_box(&mut self, ui: &mut egui::Ui, slot: &Slot) {
        ui.set_min_size(BOX_SIZE - Vec2::splat(16.0));
        ui.set_max_width(BOX_SIZE.x - 16.0);
        if slot.hidden {
            ui.label(RichText::new("Hidden player").italics().weak());
            ui.label(RichText::new("profile not looked up").small().weak());
            return;
        }
        let info = self.players.get(&slot.puuid);
        let rank = info.map(|p| p.rank.clone()).unwrap_or_default();
        let badge = match &rank {
            Fetch::Ready(r) => self.icon(IconKey::Rank(r.current.tier)),
            _ => None,
        };
        let info = self.players.get(&slot.puuid);
        ui.horizontal(|ui| {
            match badge {
                Some(tex) => ui.add(egui::Image::new(egui::load::SizedTexture::new(tex, Vec2::splat(BADGE)))),
                None => ui.allocate_exact_size(Vec2::splat(BADGE), Sense::hover()).1,
            };
            ui.vertical(|ui| {
                let name = info.and_then(|p| p.riot_id.as_deref()).unwrap_or("…");
                ui.label(RichText::new(name).strong());
                ui.label(RichText::new(self.rank_text(&rank)).small());
            });
        });
        ui.label(RichText::new(stats_line(info)).small());
    }

    fn detail_card(&mut self, ui: &mut egui::Ui, slot: &Slot) {
        ui.set_width(CARD_WIDTH);
        let info = self.players.get(&slot.puuid);
        let name = info.and_then(|p| p.riot_id.clone()).unwrap_or_else(|| "…".into());
        ui.label(RichText::new(name).strong().size(16.0));
        let rank = info.map(|p| p.rank.clone()).unwrap_or_default();
        ui.label(format!("Rank: {}", self.rank_text(&rank)));
        if let Fetch::Ready(profile) = &rank
            && let Some((tier, act)) = self.peak(profile)
        {
            ui.label(format!("Peak: {} ({act})", self.tier_label(tier)));
        }

        ui.add_space(4.0);
        let stats = info.map(|p| p.stats.clone()).unwrap_or_default();
        match &stats {
            Fetch::Ready(s) => {
                ui.label(RichText::new(format!("Last {} competitive", s.matches)).weak());
                let decided = (s.wins + s.losses).max(1);
                ui.label(format!(
                    "Win rate {:.0}%  ({}W {}L)",
                    s.wins as f32 * 100.0 / decided as f32,
                    s.wins,
                    s.losses
                ));
                ui.label(format!("ACS {:.0} · ADR {:.0} · K/D {:.2} · HS {:.0}%", s.acs, s.adr, s.kd, s.hs_pct));
            }
            Fetch::Pending => {
                ui.label(RichText::new("Loading match stats…").weak());
            }
            Fetch::Unavailable(reason) => {
                ui.label(RichText::new(format!("Match stats unavailable: {reason}")).weak());
            }
        }

        if let Fetch::Ready(profile) = &rank {
            let acts = self.recent_acts(profile, 4);
            if !acts.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new("Recent acts").weak());
                egui::Grid::new("detail_acts").num_columns(3).spacing([10.0, 2.0]).show(ui, |ui| {
                    for (label, tier, wins, games) in acts {
                        ui.label(RichText::new(label).small());
                        ui.label(RichText::new(self.tier_label(tier)).small());
                        ui.label(RichText::new(format!("{wins}W / {games}G")).small().weak());
                        ui.end_row();
                    }
                });
            }
        }
    }

    /// Slots on one side, local player first. FFA lobbies list everyone once, under `Ally`.
    fn ordered_slots(&self, side: Side) -> Vec<Slot> {
        let Some(roster) = &self.roster else { return Vec::new() };
        if roster.ffa && side == Side::Enemy {
            return Vec::new();
        }
        let mut slots: Vec<Slot> = roster.slots.iter().filter(|s| roster.ffa || s.side == side).cloned().collect();
        slots.sort_by_key(|s| !s.is_self);
        slots
    }

    pub(crate) fn rank_text(&self, rank: &Fetch<RankProfile>) -> String {
        match rank {
            Fetch::Pending => "…".into(),
            Fetch::Unavailable(_) => "Rank unavailable".into(),
            Fetch::Ready(p) if p.current.tier == 0 => "Unranked".into(),
            Fetch::Ready(p) => format!("{} · {} RR", self.tier_label(p.current.tier), p.current.rr),
        }
    }

    pub(crate) fn tier_label(&self, tier: u32) -> String {
        self.assets.as_ref().and_then(|a| a.tier_name(tier)).map_or_else(|| format!("Tier {tier}"), str::to_owned)
    }

    /// Highest tier across all acts (normalized to the current table) and the act it was reached in.
    fn peak(&self, profile: &RankProfile) -> Option<(u32, String)> {
        let assets = self.assets.as_ref()?;
        profile
            .acts
            .iter()
            .map(|a| (assets.normalize_tier(&a.act, a.peak_tier), a))
            .filter(|(tier, _)| *tier > 0)
            .max_by_key(|(tier, a)| (*tier, assets.acts.get(&a.act).map(|x| x.start.clone())))
            .map(|(tier, a)| (tier, act_label(self, &a.act)))
    }

    /// Newest acts first: (label, final tier normalized, wins, games).
    fn recent_acts(&self, profile: &RankProfile, n: usize) -> Vec<(String, u32, u32, u32)> {
        let Some(assets) = &self.assets else { return Vec::new() };
        let mut acts: Vec<_> = profile.acts.iter().collect();
        acts.sort_by(|a, b| {
            let start = |x: &str| assets.acts.get(x).map(|a| a.start.as_str()).unwrap_or("");
            start(&b.act).cmp(start(&a.act))
        });
        acts.into_iter()
            .take(n)
            .map(|a| (act_label(self, &a.act), assets.normalize_tier(&a.act, a.tier), a.wins, a.games))
            .collect()
    }
}

fn act_label(app: &App, act: &str) -> String {
    app.assets.as_ref().and_then(|a| a.acts.get(act)).map_or_else(|| "Unknown act".into(), |a| a.label.clone())
}

fn stats_line(info: Option<&PlayerInfo>) -> String {
    match info.map(|p| &p.stats) {
        Some(Fetch::Ready(s)) => {
            let decided = (s.wins + s.losses).max(1);
            format!("K/D {:.2} · HS {:.0}% · WR {:.0}%", s.kd, s.hs_pct, s.wins as f32 * 100.0 / decided as f32)
        }
        Some(Fetch::Unavailable(reason)) => format!("Stats: {reason}"),
        Some(Fetch::Pending) | None => "Loading stats…".into(),
    }
}

fn to_screen(anchor: [f32; 2], screen: Vec2) -> Pos2 {
    Pos2::new(anchor[0] * screen.x, anchor[1] * screen.y)
}
