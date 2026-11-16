//! In-game overlay: a borderless, transparent, always-on-top window covering the primary monitor.
//! Nothing is injected into the game; Valorant must run in Windowed Fullscreen for it to show.
//!
//! * Agent select: a compact box under each teammate's card; the detail card opens to the right
//!   of the card row.
//! * Loading screen: a box on every player's banner; the detail card opens toward the centre.
//! * Live match: hidden until the hotkey shows a centred scoreboard.
//!
//! Input: the overlay starts click-through. The hotkey makes it clickable (showing it if hidden)
//! and back; while clickable a toolbar offers Hide, and boxes open a detail card. The window is
//! clipped (`SetWindowRgn`) to what it draws, so the rest of the screen stays the game's, and it
//! never takes focus.

use eframe::egui::{self, Align2, Color32, Id, Pos2, Rect, RichText, Sense, Vec2, ViewportId};

use crate::config::OverlayConfig;
use crate::model::{Fetch, IconKey, Impact, RankProfile, RoundSwing, Side, Slot, Status};
use crate::val_assets::Assets;
use crate::win32::{self, Hotkey};
use crate::{App, Columns, PlayerInfo, impact_label};

/// Loading-screen box: fits inside a player's 405×114 banner.
const LOADING_BOX: Vec2 = Vec2::new(250.0, 58.0);
/// Agent-select box: one 126 px card wide, fits the strip under the card row.
const AGENT_BOX: Vec2 = Vec2::new(126.0, 36.0);
const CARD_WIDTH: f32 = 340.0;
const STRIP_HEIGHT: f32 = 34.0;
const BADGE: f32 = 24.0;
const INTERACTIVE_TITLE: &str = "vltracker overlay (interactive)";
const PASSTHROUGH_TITLE: &str = "vltracker overlay";
const ACCENT: Color32 = Color32::from_rgb(240, 200, 80);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    AgentSelect,
    Loading,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Calibrate(Layout),
    Boxes(Layout),
    Scoreboard,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum Visibility {
    #[default]
    Hidden,
    /// Shown; mouse input goes to the game.
    ClickThrough,
    /// Shown with a toolbar; boxes open detail cards.
    Clickable,
}

#[derive(Default)]
pub struct OverlayState {
    visibility: Visibility,
    calibrating: Option<Layout>,
    /// Player whose detail card is open.
    expanded: Option<String>,
    /// Native window of the current overlay viewport, once found and styled.
    window: Option<(ViewportId, isize)>,
    /// Last hit region applied, in client pixels.
    region: Option<Vec<[i32; 4]>>,
}

impl OverlayState {
    /// Agent select and the loading screen show the overlay click-through; anything else hides it.
    pub fn on_status(&mut self, status: Status) {
        self.visibility = match status {
            Status::AgentSelect | Status::Loading => Visibility::ClickThrough,
            _ => Visibility::Hidden,
        };
        self.expanded = None;
    }

    /// The hotkey: hidden or click-through → clickable; clickable → click-through. In a live match
    /// there is no cursor mid-round, so clickable → hidden instead of leaving a scoreboard over
    /// the game.
    fn toggle_clickable(&mut self, status: Status) {
        self.visibility = match (self.visibility, status) {
            (Visibility::Clickable, Status::LiveMatch) => Visibility::Hidden,
            (Visibility::Clickable, _) => Visibility::ClickThrough,
            _ => Visibility::Clickable,
        };
        if self.visibility != Visibility::Clickable {
            self.expanded = None;
        }
    }

    fn hide(&mut self) {
        self.visibility = Visibility::Hidden;
        self.expanded = None;
    }
}

fn panel_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(Color32::from_black_alpha(215))
        .stroke(egui::Stroke::new(1.0, Color32::from_white_alpha(28)))
        .corner_radius(6.0)
        .inner_margin(8.0)
}

/// Player boxes get an accent border while clickable.
fn box_frame(layout: Layout, clickable: bool) -> egui::Frame {
    let frame = match layout {
        Layout::AgentSelect => panel_frame().inner_margin(4.0).corner_radius(4.0),
        Layout::Loading => panel_frame(),
    };
    if clickable { frame.stroke(egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.7))) } else { frame }
}

fn box_size(layout: Layout) -> Vec2 {
    match layout {
        Layout::AgentSelect => AGENT_BOX,
        Layout::Loading => LOADING_BOX,
    }
}

impl App {
    pub(crate) fn on_hotkey(&mut self, key: Hotkey) {
        match key {
            Hotkey::Toggle => {
                if self.overlay.calibrating.is_none() && self.roster.is_some() {
                    self.overlay.toggle_clickable(self.status);
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
        if self.overlay.visibility == Visibility::Hidden || self.roster.is_none() {
            return None;
        }
        match self.status {
            Status::AgentSelect => Some(Mode::Boxes(Layout::AgentSelect)),
            Status::Loading => Some(Mode::Boxes(Layout::Loading)),
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
        let interactive = matches!(mode, Mode::Calibrate(_)) || self.overlay.visibility == Visibility::Clickable;
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
                Mode::Boxes(layout) => Some(self.draw_boxes(ui.ctx(), layout, screen, interactive)),
                Mode::Scoreboard => Some(vec![self.draw_scoreboard(ui.ctx())]),
            };
            let rects = rects.map(|mut rects| {
                if interactive {
                    rects.push(self.draw_toolbar(ui.ctx()));
                }
                rects
            });
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

    /// Hide button and a reminder of the hotkey; shown while the overlay is clickable.
    fn draw_toolbar(&mut self, ctx: &egui::Context) -> Rect {
        let live = self.status == Status::LiveMatch;
        let resp = egui::Area::new(Id::unique("overlay_toolbar"))
            .anchor(Align2::CENTER_TOP, Vec2::new(0.0, 8.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                panel_frame().inner_margin(6.0).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("vltracker").strong().color(ACCENT));
                        if !live {
                            ui.label(RichText::new("click a player for details").weak());
                        }
                        if ui.button("Hide").clicked() {
                            self.overlay.hide();
                        }
                        let action = if live { "hide" } else { "click-through" };
                        ui.label(RichText::new(format!("{}: {action}", self.config.overlay.hotkey)).small().weak());
                    });
                })
            });
        resp.response.rect
    }

    /// Player boxes for agent select (allies under their cards) or the loading screen (everyone on
    /// their banner), plus the open detail card.
    fn draw_boxes(&mut self, ctx: &egui::Context, layout: Layout, screen: Vec2, clickable: bool) -> Vec<Rect> {
        let cfg = &self.config.overlay;
        let groups = match layout {
            Layout::AgentSelect => vec![(self.screen_order(Side::Ally, true), cfg.agent_select.clone())],
            Layout::Loading => vec![
                (self.screen_order(Side::Ally, false), cfg.loading_ally.clone()),
                (self.screen_order(Side::Enemy, false), cfg.loading_enemy.clone()),
            ],
        };
        let card_anchor = to_screen(cfg.agent_select_card, screen);
        let mut rects = Vec::new();
        let mut open = None;
        for (g, (slots, anchors)) in groups.into_iter().enumerate() {
            for (i, (slot, anchor)) in slots.iter().zip(&anchors).enumerate() {
                let resp = egui::Area::new(Id::unique(("overlay_box", layout == Layout::Loading, g, i)))
                    .current_pos(to_screen(*anchor, screen))
                    .interactable(clickable)
                    .sense(if clickable { Sense::click() } else { Sense::hover() })
                    .order(egui::Order::Foreground)
                    .show(ctx, |ui| {
                        box_frame(layout, clickable).show(ui, |ui| match layout {
                            Layout::AgentSelect => self.compact_box(ui, slot),
                            Layout::Loading => self.stat_box(ui, slot),
                        })
                    });
                let rect = resp.response.rect;
                if clickable && !slot.hidden && resp.response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    let was_open = self.overlay.expanded.as_deref() == Some(slot.puuid.as_str());
                    self.overlay.expanded = (!was_open).then(|| slot.puuid.clone());
                }
                if self.overlay.expanded.as_deref() == Some(slot.puuid.as_str()) {
                    open = Some((slot.clone(), rect));
                }
                rects.push(rect);
            }
        }

        if let Some((slot, rect)) = open {
            let (pos, pivot) = match layout {
                Layout::AgentSelect => (card_anchor, Align2::LEFT_BOTTOM),
                // Open toward the centre of the screen, beside the box.
                Layout::Loading if rect.center().x < screen.x / 2.0 => (rect.right_top() + Vec2::new(8.0, 0.0), Align2::LEFT_TOP),
                Layout::Loading => (rect.left_top() - Vec2::new(8.0, 0.0), Align2::RIGHT_TOP),
            };
            let card = egui::Area::new(Id::unique("detail_card"))
                .current_pos(pos)
                .pivot(pivot)
                .constrain(true)
                .order(egui::Order::Foreground)
                .show(ctx, |ui| panel_frame().show(ui, |ui| self.detail_card(ui, &slot)));
            rects.push(card.response.rect);
        }
        rects
    }

    fn draw_scoreboard(&mut self, ctx: &egui::Context) -> Rect {
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
                    self.grouped_grid(ui, "scoreboard_grid", groups, Columns::Scoreboard);
                });
            });
        resp.response.rect
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
                                Layout::AgentSelect => {
                                    cfg.agent_select = defaults.agent_select;
                                    cfg.agent_select_card = defaults.agent_select_card;
                                }
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

        let size = box_size(layout);
        let cfg = &mut self.config.overlay;
        let groups: Vec<(&str, &mut Vec<[f32; 2]>)> = match layout {
            Layout::AgentSelect => vec![("Teammate", &mut cfg.agent_select)],
            Layout::Loading => vec![("Ally", &mut cfg.loading_ally), ("Enemy", &mut cfg.loading_enemy)],
        };
        for (label, anchors) in groups {
            for (i, anchor) in anchors.iter_mut().enumerate() {
                let text = format!("{label} {}", i + 1);
                drag_placeholder(ctx, ("calibrate_box", label, i), anchor, screen, size, Align2::LEFT_TOP, &text);
            }
        }
        if layout == Layout::AgentSelect {
            let size = Vec2::new(CARD_WIDTH, 160.0);
            let anchor = &mut cfg.agent_select_card;
            drag_placeholder(ctx, "calibrate_card", anchor, screen, size, Align2::LEFT_BOTTOM, "Detail card");
        }
    }

    /// Two-line box for the strip under an agent-select card: name and impact, then rank.
    fn compact_box(&mut self, ui: &mut egui::Ui, slot: &Slot) {
        ui.set_min_size(AGENT_BOX - Vec2::splat(8.0));
        ui.set_max_width(AGENT_BOX.x - 8.0);
        ui.spacing_mut().item_spacing = Vec2::new(4.0, 1.0);
        if slot.hidden {
            ui.label(RichText::new("Hidden player").small().italics().weak());
            return;
        }
        let info = self.players.get(&slot.puuid);
        let name = info.and_then(|p| p.riot_id.as_deref()).map_or("…", |id| id.split('#').next().unwrap_or(id));
        let rating = match info.map(|p| &p.impact) {
            Some(Fetch::Ready(i)) => RichText::new(format!("{:.2}", i.rating)).strong().color(impact_label(i.rating).1),
            Some(Fetch::Unavailable(_)) => RichText::new("—").weak(),
            Some(Fetch::Pending) | None => RichText::new("…").weak(),
        };
        // Name truncates to leave room for the rating; the row stays one line tall.
        egui::Sides::new().shrink_left().truncate().show(
            ui,
            |ui| ui.label(RichText::new(name).small().strong()),
            |ui| ui.label(rating.small()),
        );
        let rank = info.map(|p| p.rank.clone()).unwrap_or_default();
        ui.add(egui::Label::new(RichText::new(self.short_rank(&rank)).small().weak()).truncate());
    }

    /// Compact per-player box: rank badge, name, rank, and headline stats.
    fn stat_box(&mut self, ui: &mut egui::Ui, slot: &Slot) {
        ui.set_min_size(LOADING_BOX - Vec2::splat(16.0));
        ui.set_max_width(LOADING_BOX.x - 16.0);
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
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            match info.map(|p| &p.impact) {
                Some(Fetch::Ready(i)) => {
                    let (_, color) = impact_label(i.rating);
                    ui.label(RichText::new(format!("Impact {:.2}", i.rating)).small().strong().color(color));
                }
                Some(Fetch::Unavailable(_)) => {
                    ui.label(RichText::new("Impact —").small().weak());
                }
                Some(Fetch::Pending) | None => {
                    ui.label(RichText::new("Impact …").small().weak());
                }
            }
            ui.label(RichText::new(stats_line(info)).small());
        });
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
        match info.map(|p| p.impact.clone()).unwrap_or_default() {
            Fetch::Ready(i) => impact_section(ui, &i, self.assets.as_deref()),
            Fetch::Pending => {
                ui.label(RichText::new("Calculating impact from the last 10 matches…").weak());
            }
            Fetch::Unavailable(reason) => {
                ui.label(RichText::new(format!("Impact unavailable: {reason}")).weak());
            }
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

    /// Players on one side in the order Valorant draws them: the server's roster order, with the
    /// local player moved to the middle card in agent select. (Matches the reference screenshots;
    /// each box shows the player's name so a different order would be obvious.)
    fn screen_order(&self, side: Side, center_self: bool) -> Vec<Slot> {
        let Some(roster) = &self.roster else { return Vec::new() };
        let mut slots: Vec<Slot> = roster.slots.iter().filter(|s| roster.ffa || s.side == side).cloned().collect();
        if roster.ffa && side == Side::Enemy {
            return Vec::new();
        }
        if center_self {
            move_self_to_middle(&mut slots);
        }
        slots
    }

    /// "Ascendant 1 · 45 RR" → "Asc 1 · 45 RR" for the narrow agent-select box.
    fn short_rank(&self, rank: &Fetch<RankProfile>) -> String {
        match rank {
            Fetch::Ready(p) if p.current.tier > 0 => {
                let name = self.tier_label(p.current.tier);
                let short = match name.split_once(' ') {
                    Some((tier, div)) => format!("{} {div}", tier.chars().take(3).collect::<String>()),
                    None => name,
                };
                format!("{short} · {} RR", p.current.rr)
            }
            other => self.rank_text(other),
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

fn impact_section(ui: &mut egui::Ui, i: &Impact, assets: Option<&Assets>) {
    let (label, color) = impact_label(i.rating);
    let agent = i.main_agent.as_deref();
    let role = agent.and_then(|a| assets?.agent_role(a));
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Impact {:.2}", i.rating)).strong().size(15.0).color(color));
        ui.label(RichText::new(label).color(color));
        if let (Some(role), Some(name)) = (role, agent.and_then(|a| assets?.agent_name(a))) {
            ui.label(RichText::new(format!("· {role} ({name})")).weak());
        }
    });
    ui.label(
        RichText::new(format!(
            "{:+.1}% round-win probability per round · {} rounds, last {} competitive",
            i.wpa_per_round * 100.0,
            i.rounds,
            i.matches
        ))
        .small()
        .weak(),
    );
    round_strip(ui, &i.recent_rounds);

    ui.label(format!(
        "Opening duels {}–{} · KAST {:.0}% · Clutches {}/{} · Impact frags {}",
        i.opening_wins,
        i.opening_losses,
        i.kast * 100.0,
        i.clutches_won,
        i.clutches,
        i.impact_frags
    ))
    .on_hover_text("Impact frags: kills after the opening duel that swung the round by 25+ points");

    // Role-specific lines; the one that matters most for the role goes first and bold.
    let traded = i.deaths_traded as f32 * 100.0 / i.deaths.max(1) as f32;
    let trades = (
        format!(
            "Trades {} instant · {} late · deaths traded {traded:.0}% · missed refrags {}",
            i.trades_fast, i.trades_late, i.missed_refrags
        ),
        "Instant: within 2 s of the teammate's death; late: 2–5 s. Missed refrag: a teammate died within \
         ~15 m of you and their killer survived the next 5 s.",
    );
    let util = (
        format!(
            "Utility: {} util assists · {} ability kills · {:.1} casts/rd{}",
            i.util_assists,
            i.ability_kills,
            i.casts_per_round,
            i.agent_casts_per_round.map(|a| format!(" (agent avg {a:.1})")).unwrap_or_default()
        ),
        "Util assists: assists on players you never damaged that round (flashes, debuffs). \
         Casts are match totals; usage alone isn't rated.",
    );
    let entry = (i.entry_wins + i.entry_losses > 0).then(|| {
        let rate = i.entry_wins as f32 * 100.0 / (i.entry_wins + i.entry_losses) as f32;
        let spot = i.entry_spot.as_deref().map(|s| format!(", mostly {s}")).unwrap_or_default();
        (
            format!("Site entries {}W–{}L ({rate:.0}%){spot}", i.entry_wins, i.entry_losses),
            "The attackers' first fight inside a site area before the plant, won or lost by this player.",
        )
    });
    let lurk = (i.lurk_rounds > 0).then(|| {
        (
            format!(
                "Lurks {} rounds · {:+.1}%/rd swing · {} late kills · {} untraded deaths",
                i.lurk_rounds,
                i.lurk_swing * 100.0,
                i.lurk_late_kills,
                i.lurk_deaths_untraded
            ),
            "Attack rounds where your own fight (or a solo plant) happened ~30 m+ from every teammate before \
             the plant. Late kills: isolated kills after the round was already decided.",
        )
    });
    let mut lines: Vec<(String, &str)> = [Some(trades), Some(util), entry, lurk].into_iter().flatten().collect();
    let lead = match role {
        Some("Duelist") => lines.iter().position(|(l, _)| l.starts_with("Site entries")),
        Some("Sentinel") => lines.iter().position(|(l, _)| l.starts_with("Lurks")),
        Some("Initiator" | "Controller") => lines.iter().position(|(l, _)| l.starts_with("Utility")),
        _ => None,
    };
    if let Some(lead) = lead {
        let (text, hint) = lines.remove(lead);
        ui.label(RichText::new(text).strong()).on_hover_text(hint);
    }
    for (text, hint) in lines {
        ui.label(text).on_hover_text(hint);
    }

    if i.kills > 0 {
        let low = i.low_impact_kills as f32 * 100.0 / i.kills as f32;
        ui.label(format!("Low-impact kills {low:.0}% of {}", i.kills))
            .on_hover_text("Kills that moved round-win odds by under 3 points: exit frags, eco kills, …");
    }
    if let Some(note) = impact_note(i) {
        ui.label(RichText::new(note).italics().color(color));
    }
}

/// One bar per round (oldest left): up = round-win probability added, down = lost. A tick under
/// each bar marks rounds the team won; a gap separates matches. Hover for the round.
fn round_strip(ui: &mut egui::Ui, rounds: &[RoundSwing]) {
    if rounds.is_empty() {
        return;
    }
    let (rect, response) = ui.allocate_exact_size(Vec2::new(CARD_WIDTH, STRIP_HEIGHT), Sense::hover());
    let painter = ui.painter_at(rect);
    let gaps = rounds.iter().filter(|r| r.match_start).count().saturating_sub(1) as f32;
    let slot = ((rect.width() - gaps * 4.0) / rounds.len() as f32).min(10.0);
    let mid = rect.center().y - 3.0;
    let half = STRIP_HEIGHT / 2.0 - 5.0;
    painter.hline(rect.x_range(), mid, egui::Stroke::new(1.0, Color32::from_white_alpha(30)));

    let mut x = rect.left();
    let mut hovered = None;
    let mut round_no = 0;
    for (idx, r) in rounds.iter().enumerate() {
        if r.match_start {
            if idx > 0 {
                x += 4.0;
            }
            round_no = 0;
        }
        round_no += 1;
        let height = (r.swing.abs() / 0.5).min(1.0) * half;
        let (top, bottom) = if r.swing >= 0.0 { (mid - height, mid) } else { (mid, mid + height) };
        let color = if r.swing >= 0.0 { Color32::from_rgb(90, 210, 120) } else { Color32::from_rgb(230, 110, 100) };
        let bar = Rect::from_min_max(Pos2::new(x + 1.0, top), Pos2::new(x + slot - 1.0, bottom.max(top + 1.0)));
        painter.rect_filled(bar, 1.0, color);
        let tick = if r.won { Color32::from_white_alpha(140) } else { Color32::from_white_alpha(25) };
        let tick_rect = Rect::from_min_max(Pos2::new(x + 1.0, rect.bottom() - 3.0), Pos2::new(x + slot - 1.0, rect.bottom()));
        painter.rect_filled(tick_rect, 0.0, tick);
        if response.hover_pos().is_some_and(|p| p.x >= x && p.x < x + slot) {
            hovered = Some((round_no, r));
        }
        x += slot;
    }
    if let Some((n, r)) = hovered {
        let mut text = format!("Round {n}: {:+.1}% · {} kill{}", r.swing * 100.0, r.kills, if r.kills == 1 { "" } else { "s" });
        for (flag, label) in [(r.died, "died"), (r.planted, "planted"), (r.defused, "defused")] {
            if flag {
                text.push_str(&format!(" · {label}"));
            }
        }
        text.push_str(if r.won { " · won" } else { " · lost" });
        response.on_hover_text_at_pointer(text);
    }
}

/// Calls out where the scoreboard and the impact disagree.
fn impact_note(i: &Impact) -> Option<&'static str> {
    let kd = i.kills as f32 / i.deaths.max(1) as f32;
    let low_share = i.low_impact_kills as f32 / i.kills.max(1) as f32;
    if kd >= 1.10 && i.rating < 0.95 {
        Some("Kills aren't turning into rounds won")
    } else if kd < 0.95 && i.rating >= 1.07 {
        Some("Wins rounds beyond what K/D shows")
    } else if i.kills >= 10 && low_share >= 0.40 {
        Some("Many kills come after the round is decided or against weaker buys")
    } else {
        None
    }
}

fn stats_line(info: Option<&PlayerInfo>) -> String {
    match info.map(|p| &p.stats) {
        Some(Fetch::Ready(s)) => format!("· K/D {:.2} · HS {:.0}%", s.kd, s.hs_pct),
        Some(Fetch::Unavailable(reason)) => format!("· stats: {reason}"),
        Some(Fetch::Pending) | None => "· loading stats…".into(),
    }
}

fn to_screen(anchor: [f32; 2], screen: Vec2) -> Pos2 {
    Pos2::new(anchor[0] * screen.x, anchor[1] * screen.y)
}

/// Valorant draws the local player's card in the middle of the agent-select row.
fn move_self_to_middle(slots: &mut Vec<Slot>) {
    if let Some(i) = slots.iter().position(|s| s.is_self) {
        let me = slots.remove(i);
        slots.insert(slots.len() / 2, me);
    }
}

/// A draggable stand-in for calibration; writes the new position back to `anchor`.
fn drag_placeholder(
    ctx: &egui::Context,
    id: impl std::hash::Hash + std::fmt::Debug,
    anchor: &mut [f32; 2],
    screen: Vec2,
    size: Vec2,
    pivot: Align2,
    label: &str,
) {
    let resp = egui::Area::new(Id::unique(id))
        .current_pos(to_screen(*anchor, screen))
        .pivot(pivot)
        .sense(Sense::drag())
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            panel_frame().inner_margin(4.0).stroke(egui::Stroke::new(1.5, ACCENT)).show(ui, |ui| {
                ui.set_min_size(size - Vec2::splat(8.0));
                ui.set_max_width(size.x - 8.0);
                ui.label(RichText::new(label).small().strong());
                ui.label(RichText::new("drag").small().weak());
            })
        });
    let delta = resp.response.drag_delta();
    if delta != Vec2::ZERO {
        let pos = to_screen(*anchor, screen) + delta;
        *anchor = [(pos.x / screen.x).clamp(0.0, 1.0), (pos.y / screen.y).clamp(0.0, 1.0)];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_toggles_clickable_and_hide_is_separate() {
        let mut o = OverlayState::default();
        o.on_status(Status::AgentSelect);
        assert_eq!(o.visibility, Visibility::ClickThrough, "agent select shows the overlay click-through");
        o.toggle_clickable(Status::AgentSelect);
        assert_eq!(o.visibility, Visibility::Clickable);
        o.expanded = Some("p".into());
        o.toggle_clickable(Status::AgentSelect);
        assert_eq!((o.visibility, o.expanded.as_deref()), (Visibility::ClickThrough, None));
        o.toggle_clickable(Status::AgentSelect);
        o.hide();
        assert_eq!(o.visibility, Visibility::Hidden);
        o.toggle_clickable(Status::AgentSelect);
        assert_eq!(o.visibility, Visibility::Clickable, "the hotkey brings a hidden overlay back clickable");

        o.on_status(Status::LiveMatch);
        assert_eq!(o.visibility, Visibility::Hidden, "the match starts with the overlay hidden");
        o.toggle_clickable(Status::LiveMatch);
        assert_eq!(o.visibility, Visibility::Clickable);
        o.toggle_clickable(Status::LiveMatch);
        assert_eq!(o.visibility, Visibility::Hidden, "no click-through scoreboard left over the game");
    }

    #[test]
    fn local_player_moves_to_the_middle_card() {
        let slot = |p: &str, me: bool| Slot {
            puuid: p.into(),
            side: Side::Ally,
            agent: None,
            locked: false,
            hidden: false,
            is_self: me,
        };
        let mut slots = vec![slot("me", true), slot("a", false), slot("b", false), slot("c", false), slot("d", false)];
        move_self_to_middle(&mut slots);
        let order: Vec<&str> = slots.iter().map(|s| s.puuid.as_str()).collect();
        assert_eq!(order, ["a", "b", "me", "c", "d"]);
    }
}
