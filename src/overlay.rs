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
use crate::details::CARD_WIDTH;
use crate::model::{Fetch, IconKey, RankProfile, Side, Slot, Status};
use crate::win32::{self, Hotkey};
use crate::{App, Columns, PlayerInfo, impact_label};

/// Loading-screen box: fits inside a player's 405×114 banner.
const LOADING_BOX: Vec2 = Vec2::new(250.0, 58.0);
/// Agent-select box: one 126 px card wide, with room for both lines and the frame.
const AGENT_BOX: Vec2 = Vec2::new(126.0, 44.0);
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
    pub(crate) fn is_calibrating(&self) -> bool {
        self.calibrating.is_some()
    }

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
fn box_frame(layout: Layout, clickable: bool, scale: f32) -> egui::Frame {
    let frame = match layout {
        Layout::AgentSelect => panel_frame().inner_margin(4.0 * scale).corner_radius(4.0),
        Layout::Loading => panel_frame().inner_margin(8.0 * scale),
    };
    if clickable {
        frame.stroke(egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.7)))
    } else {
        frame
    }
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
            let layout = if self.status == Status::Loading {
                Layout::Loading
            } else {
                Layout::AgentSelect
            };
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
        let interactive =
            matches!(mode, Mode::Calibrate(_)) || self.overlay.visibility == Visibility::Clickable;
        let title = if interactive {
            INTERACTIVE_TITLE
        } else {
            PASSTHROUGH_TITLE
        };
        // Separate viewports per input mode: winit rewrites the window's extended style whenever its
        // flags change, so the click-through setting is fixed at creation instead of toggled.
        let id = ViewportId::from_hash_of(title);

        // The main window can be on another monitor, with a different DPI. Its scale only
        // bootstraps the viewport builder; all drawing below uses the overlay's own viewport.
        let main_ppp = ctx
            .input(|i| i.viewport().native_pixels_per_point)
            .unwrap_or(1.0);
        let requested_size = win32::primary_monitor_px()
            .map(|[w, h]| Vec2::new(w, h) / main_ppp)
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
            .with_inner_size(requested_size)
            .with_mouse_passthrough(!interactive);

        ctx.show_viewport_immediate(id, builder, |ui, _| {
            let ctx = ui.ctx();
            self.style_overlay_window(ctx, id, title, interactive);
            let screen = ctx.viewport_rect().size();
            let rects = match mode {
                Mode::Calibrate(layout) => {
                    self.draw_calibration(ctx, layout, screen);
                    None
                }
                Mode::Boxes(layout) => Some(self.draw_boxes(ctx, layout, screen, interactive)),
                Mode::Scoreboard => Some(vec![self.draw_scoreboard(ctx)]),
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

    fn style_overlay_window(
        &mut self,
        ctx: &egui::Context,
        id: ViewportId,
        title: &str,
        interactive: bool,
    ) {
        if self.overlay.window.is_some_and(|(v, _)| v == id) {
            return;
        }
        match win32::find_window(title) {
            Some(hwnd) => {
                if interactive {
                    win32::set_no_activate(hwnd);
                }
                win32::cover_primary_monitor(hwnd);
                ctx.request_repaint();
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
        let Some((_, hwnd)) = self.overlay.window else {
            return;
        };
        let ppp = ctx.pixels_per_point();
        let origin = ctx.viewport_rect().min;
        let px = rects.map(|rects| {
            rects
                .iter()
                .map(|r| {
                    let r = r.expand(2.0).translate(-origin.to_vec2());
                    [
                        (r.min.x * ppp).floor() as i32,
                        (r.min.y * ppp).floor() as i32,
                        (r.max.x * ppp).ceil() as i32,
                        (r.max.y * ppp).ceil() as i32,
                    ]
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
                        ui.label(
                            RichText::new(format!("{}: {action}", self.config.overlay.hotkey))
                                .small()
                                .weak(),
                        );
                    });
                })
            });
        resp.response.rect
    }

    /// Player boxes for agent select (allies under their cards) or the loading screen (everyone on
    /// their banner), plus the open detail card.
    fn draw_boxes(
        &mut self,
        ctx: &egui::Context,
        layout: Layout,
        screen: Vec2,
        clickable: bool,
    ) -> Vec<Rect> {
        let cfg = &self.config.overlay;
        let groups = match layout {
            Layout::AgentSelect => vec![(
                self.screen_order(Side::Ally, true),
                cfg.agent_select.clone(),
            )],
            Layout::Loading => vec![
                (
                    self.screen_order(Side::Ally, false),
                    cfg.loading_ally.clone(),
                ),
                (
                    self.screen_order(Side::Enemy, false),
                    cfg.loading_enemy.clone(),
                ),
            ],
        };
        let card_anchor = to_screen(cfg.agent_select_card, screen);
        let mut rects = Vec::new();
        let mut open = None;
        for (g, (slots, anchors)) in groups.into_iter().enumerate() {
            for (i, (slot, anchor)) in slots.iter().zip(&anchors).enumerate() {
                let scale = canvas(screen).1;
                let resp =
                    egui::Area::new(Id::unique(("overlay_box", layout == Layout::Loading, g, i)))
                        .current_pos(visible_anchor(
                            to_screen(*anchor, screen),
                            box_size(layout) * scale,
                            screen,
                            Align2::LEFT_TOP,
                        ))
                        .movable(false)
                        .interactable(clickable)
                        .sense(if clickable {
                            Sense::click()
                        } else {
                            Sense::hover()
                        })
                        .order(egui::Order::Foreground)
                        .constrain_to(ctx.viewport_rect())
                        .show(ctx, |ui| {
                            scale_text(ui, scale);
                            box_frame(layout, clickable, scale).show(ui, |ui| match layout {
                                Layout::AgentSelect => self.compact_box(ui, slot, scale),
                                Layout::Loading => self.stat_box(ui, slot, scale),
                            })
                        });
                let rect = resp.response.rect;
                if clickable
                    && !slot.hidden
                    && resp
                        .response
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                {
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
                Layout::Loading if rect.center().x < ctx.viewport_rect().center().x => {
                    (rect.right_top() + Vec2::new(8.0, 0.0), Align2::LEFT_TOP)
                }
                Layout::Loading => (rect.left_top() - Vec2::new(8.0, 0.0), Align2::RIGHT_TOP),
            };
            let card = egui::Area::new(Id::unique("detail_card"))
                .current_pos(pos)
                .pivot(pivot)
                .constrain(true)
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    panel_frame().show(ui, |ui| self.detail_card(ui, &slot.puuid))
                });
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
                    let (allies, enemies) = (
                        self.ordered_slots(Side::Ally),
                        self.ordered_slots(Side::Enemy),
                    );
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
                        if ui
                            .selectable_label(layout == Layout::AgentSelect, "Agent select")
                            .clicked()
                        {
                            self.overlay.calibrating = Some(Layout::AgentSelect);
                        }
                        if ui
                            .selectable_label(layout == Layout::Loading, "Loading screen")
                            .clicked()
                        {
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

        let size = box_size(layout) * canvas(screen).1;
        let cfg = &mut self.config.overlay;
        let groups: Vec<(&str, &mut Vec<[f32; 2]>)> = match layout {
            Layout::AgentSelect => vec![("Teammate", &mut cfg.agent_select)],
            Layout::Loading => vec![
                ("Ally", &mut cfg.loading_ally),
                ("Enemy", &mut cfg.loading_enemy),
            ],
        };
        for (label, anchors) in groups {
            for (i, anchor) in anchors.iter_mut().enumerate() {
                let text = format!("{label} {}", i + 1);
                drag_placeholder(
                    ctx,
                    ("calibrate_box", label, i),
                    anchor,
                    screen,
                    size,
                    Align2::LEFT_TOP,
                    &text,
                );
            }
        }
        if layout == Layout::AgentSelect {
            let size = Vec2::new(CARD_WIDTH + 16.0, 160.0);
            let anchor = &mut cfg.agent_select_card;
            drag_placeholder(
                ctx,
                "calibrate_card",
                anchor,
                screen,
                size,
                Align2::LEFT_BOTTOM,
                "Detail card",
            );
        }
    }

    /// Two-line box for the strip under an agent-select card: name and impact, then rank.
    fn compact_box(&mut self, ui: &mut egui::Ui, slot: &Slot, scale: f32) {
        ui.set_min_size((AGENT_BOX - Vec2::splat(8.0)) * scale);
        ui.set_max_width((AGENT_BOX.x - 8.0) * scale);
        ui.spacing_mut().item_spacing = Vec2::new(4.0, 1.0) * scale;
        if slot.hidden {
            ui.label(RichText::new("Hidden player").small().italics().weak());
            return;
        }
        let info = self.players.get(&slot.puuid);
        let name = info
            .and_then(|p| p.riot_id.as_deref())
            .map_or("…", |id| id.split('#').next().unwrap_or(id));
        let rating = match info.map(|p| &p.impact) {
            Some(Fetch::Ready(i)) => RichText::new(format!("{:.2}", i.rating))
                .strong()
                .color(impact_label(i.rating).1),
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
    fn stat_box(&mut self, ui: &mut egui::Ui, slot: &Slot, scale: f32) {
        ui.set_min_size((LOADING_BOX - Vec2::splat(16.0)) * scale);
        ui.set_max_width((LOADING_BOX.x - 16.0) * scale);
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
                Some(tex) => ui.add(egui::Image::new(egui::load::SizedTexture::new(
                    tex,
                    Vec2::splat(BADGE * scale),
                ))),
                None => {
                    ui.allocate_exact_size(Vec2::splat(BADGE * scale), Sense::hover())
                        .1
                }
            };
            ui.vertical(|ui| {
                // Long names truncate instead of widening the box past the banner.
                let name = info.and_then(|p| p.riot_id.as_deref()).unwrap_or("…");
                ui.add(egui::Label::new(RichText::new(name).strong()).truncate());
                ui.add(egui::Label::new(RichText::new(self.rank_text(&rank)).small()).truncate());
            });
        });
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0 * scale;
            match info.map(|p| &p.impact) {
                Some(Fetch::Ready(i)) => {
                    let (_, color) = impact_label(i.rating);
                    ui.label(
                        RichText::new(format!("Impact {:.2}", i.rating))
                            .small()
                            .strong()
                            .color(color),
                    );
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

    /// Players on one side in the order Valorant draws them: the server's roster order, with the
    /// local player moved to the middle card in agent select. (Matches the reference screenshots;
    /// each box shows the player's name so a different order would be obvious.)
    fn screen_order(&self, side: Side, center_self: bool) -> Vec<Slot> {
        let Some(roster) = &self.roster else {
            return Vec::new();
        };
        let mut slots: Vec<Slot> = roster
            .slots
            .iter()
            .filter(|s| roster.ffa || s.side == side)
            .cloned()
            .collect();
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
                    Some((tier, div)) => {
                        format!("{} {div}", tier.chars().take(3).collect::<String>())
                    }
                    None => name,
                };
                format!("{short} · {} RR", p.current.rr)
            }
            other => self.rank_text(other),
        }
    }

    /// Slots on one side, local player first. FFA lobbies list everyone once, under `Ally`.
    fn ordered_slots(&self, side: Side) -> Vec<Slot> {
        let Some(roster) = &self.roster else {
            return Vec::new();
        };
        if roster.ffa && side == Side::Enemy {
            return Vec::new();
        }
        let mut slots: Vec<Slot> = roster
            .slots
            .iter()
            .filter(|s| roster.ffa || s.side == side)
            .cloned()
            .collect();
        slots.sort_by_key(|s| !s.is_self);
        slots
    }
}

fn stats_line(info: Option<&PlayerInfo>) -> String {
    match info.map(|p| &p.stats) {
        Some(Fetch::Ready(s)) => format!("· K/D {:.2} · HS {:.0}%", s.kd, s.hs_pct),
        Some(Fetch::Unavailable(reason)) => format!("· stats: {reason}"),
        Some(Fetch::Pending) | None => "· loading stats…".into(),
    }
}

/// Map reference-screen fractions into a centred 16:9 game canvas. On 16:10 and ultrawide
/// monitors the extra space stays at the edges instead of stretching player-card spacing.
fn canvas(screen: Vec2) -> (Pos2, f32) {
    let scale = (screen.x / 1920.0).min(screen.y / 1080.0);
    (
        Pos2::new(
            (screen.x - 1920.0 * scale) / 2.0,
            (screen.y - 1080.0 * scale) / 2.0,
        ),
        scale,
    )
}

fn to_screen(anchor: [f32; 2], screen: Vec2) -> Pos2 {
    let (origin, scale) = canvas(screen);
    origin + Vec2::new(anchor[0] * 1920.0 * scale, anchor[1] * 1080.0 * scale)
}

fn from_screen(pos: Pos2, screen: Vec2) -> [f32; 2] {
    let (origin, scale) = canvas(screen);
    [
        (pos.x - origin.x) / (1920.0 * scale),
        (pos.y - origin.y) / (1080.0 * scale),
    ]
}

/// Keep the full rendered box visible, including when a calibrated anchor came from an older
/// screen size. The pivot decides which edge is represented by the saved anchor.
fn visible_anchor(pos: Pos2, size: Vec2, screen: Vec2, pivot: Align2) -> Pos2 {
    let right = pivot == Align2::RIGHT_TOP || pivot == Align2::RIGHT_BOTTOM;
    let bottom = pivot == Align2::LEFT_BOTTOM || pivot == Align2::RIGHT_BOTTOM;
    Pos2::new(
        pos.x.clamp(
            if right { size.x.min(screen.x) } else { 0.0 },
            if right {
                screen.x
            } else {
                (screen.x - size.x).max(0.0)
            },
        ),
        pos.y.clamp(
            if bottom { size.y.min(screen.y) } else { 0.0 },
            if bottom {
                screen.y
            } else {
                (screen.y - size.y).max(0.0)
            },
        ),
    )
}

/// Valorant draws the local player's card in the middle of the agent-select row.
fn move_self_to_middle(slots: &mut Vec<Slot>) {
    if let Some(i) = slots.iter().position(|s| s.is_self) {
        let me = slots.remove(i);
        slots.insert(slots.len() / 2, me);
    }
}

/// Scale the box text with the game canvas, not with the main window's DPI. This keeps adjacent
/// agent cards distinct at 720p and text aligned with their banners at 1440p and above.
fn scale_text(ui: &mut egui::Ui, scale: f32) {
    if scale != 1.0 {
        for font in ui.style_mut().text_styles.values_mut() {
            font.size *= scale;
        }
        ui.spacing_mut().item_spacing *= scale;
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
    let scale = canvas(screen).1;
    let pos = visible_anchor(to_screen(*anchor, screen), size, screen, pivot);
    let resp = egui::Area::new(Id::unique(id))
        .current_pos(pos)
        .pivot(pivot)
        .movable(false)
        .sense(Sense::drag())
        .order(egui::Order::Foreground)
        .constrain_to(ctx.viewport_rect())
        .show(ctx, |ui| {
            scale_text(ui, scale);
            panel_frame()
                .inner_margin(4.0 * scale)
                .stroke(egui::Stroke::new(1.5, ACCENT))
                .show(ui, |ui| {
                    ui.set_min_size(size - Vec2::splat(8.0 * scale));
                    ui.set_max_width(size.x - 8.0 * scale);
                    ui.label(RichText::new(label).small().strong());
                    ui.label(RichText::new("drag").small().weak());
                })
        });
    let delta = resp.response.drag_delta();
    if delta != Vec2::ZERO {
        let pos = visible_anchor(pos + delta, resp.response.rect.size(), screen, pivot);
        *anchor = from_screen(pos, screen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_toggles_clickable_and_hide_is_separate() {
        let mut o = OverlayState::default();
        o.on_status(Status::AgentSelect);
        assert_eq!(
            o.visibility,
            Visibility::ClickThrough,
            "agent select shows the overlay click-through"
        );
        o.toggle_clickable(Status::AgentSelect);
        assert_eq!(o.visibility, Visibility::Clickable);
        o.expanded = Some("p".into());
        o.toggle_clickable(Status::AgentSelect);
        assert_eq!(
            (o.visibility, o.expanded.as_deref()),
            (Visibility::ClickThrough, None)
        );
        o.toggle_clickable(Status::AgentSelect);
        o.hide();
        assert_eq!(o.visibility, Visibility::Hidden);
        o.toggle_clickable(Status::AgentSelect);
        assert_eq!(
            o.visibility,
            Visibility::Clickable,
            "the hotkey brings a hidden overlay back clickable"
        );

        o.on_status(Status::LiveMatch);
        assert_eq!(
            o.visibility,
            Visibility::Hidden,
            "the match starts with the overlay hidden"
        );
        o.toggle_clickable(Status::LiveMatch);
        assert_eq!(o.visibility, Visibility::Clickable);
        o.toggle_clickable(Status::LiveMatch);
        assert_eq!(
            o.visibility,
            Visibility::Hidden,
            "no click-through scoreboard left over the game"
        );
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
        let mut slots = vec![
            slot("me", true),
            slot("a", false),
            slot("b", false),
            slot("c", false),
            slot("d", false),
        ];
        move_self_to_middle(&mut slots);
        let order: Vec<&str> = slots.iter().map(|s| s.puuid.as_str()).collect();
        assert_eq!(order, ["a", "b", "me", "c", "d"]);
    }

    #[test]
    fn centred_game_canvas_and_drag_round_trip() {
        let config = OverlayConfig::default();
        for screen in [
            Vec2::new(1920.0, 1080.0),
            Vec2::new(1280.0, 720.0),
            Vec2::new(1536.0, 864.0),
            Vec2::new(1280.0, 800.0),
            Vec2::new(1920.0, 1200.0),
        ] {
            let first = to_screen(config.agent_select[0], screen);
            let last = to_screen(config.agent_select[4], screen);
            let scale = canvas(screen).1;
            assert!(first.x >= 0.0 && last.x + AGENT_BOX.x * scale <= screen.x);
            assert!(last.y + AGENT_BOX.y * scale <= screen.y);
            let ally = to_screen(config.loading_ally[0], screen);
            let enemy = to_screen(config.loading_enemy[4], screen);
            assert!(ally.x >= 0.0 && enemy.x + LOADING_BOX.x * scale <= screen.x);
            assert!(enemy.y + LOADING_BOX.y * scale <= screen.y);
            assert!(to_screen(config.agent_select[1], screen).x >= first.x + AGENT_BOX.x * scale);
            assert!(to_screen(config.loading_ally[1], screen).y >= ally.y + LOADING_BOX.y * scale);
            assert!((from_screen(first, screen)[0] - config.agent_select[0][0]).abs() < 0.00001);
            assert!((from_screen(first, screen)[1] - config.agent_select[0][1]).abs() < 0.00001);
        }
        let screen = Vec2::new(1920.0, 1200.0);
        assert_eq!(to_screen([0.0, 0.0], screen), Pos2::new(0.0, 60.0));
        assert_eq!(
            visible_anchor(
                Pos2::new(1900.0, 1190.0),
                AGENT_BOX,
                screen,
                Align2::LEFT_TOP
            ),
            Pos2::new(1794.0, 1156.0)
        );
        assert_eq!(
            visible_anchor(
                Pos2::new(10.0, 1190.0),
                LOADING_BOX,
                screen,
                Align2::RIGHT_TOP
            ),
            Pos2::new(250.0, 1142.0)
        );
        assert_eq!(
            visible_anchor(
                Pos2::new(1900.0, 10.0),
                AGENT_BOX,
                screen,
                Align2::LEFT_BOTTOM
            ),
            Pos2::new(1794.0, 44.0)
        );
    }
}
