//! Player detail card: rank history, impact breakdown with a round-by-round strip, and recent
//! match stats. Shared by the overlay (agent select / loading screen) and the main window.

use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, Vec2};

use crate::impact_label;
use crate::model::{Depth, Fetch, Impact, Progress, RankProfile, RoundSwing, Stats};
use crate::{App, PlayerInfo};

pub(crate) const CARD_WIDTH: f32 = 340.0;
const STRIP_HEIGHT: f32 = 34.0;

/// "1 kill", "3 kills".
fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn percent(part: u32, whole: u32) -> f32 {
    part as f32 * 100.0 / whole.max(1) as f32
}

/// Small heading with a rule under it.
fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(title).small().strong().color(Color32::from_gray(150)));
    ui.separator();
}

/// One label/value row. `lead` bolds rows that matter most for the player's role.
struct Row {
    label: &'static str,
    value: String,
    hint: &'static str,
    lead: bool,
}

fn row(label: &'static str, value: String, hint: &'static str) -> Row {
    Row { label, value, hint, lead: false }
}

fn rows_grid(ui: &mut egui::Ui, id: &str, rows: &[Row]) {
    egui::Grid::new(id).num_columns(2).spacing([14.0, 3.0]).show(ui, |ui| {
        for r in rows {
            let label = RichText::new(r.label).color(Color32::from_gray(if r.lead { 220 } else { 160 }));
            let value = RichText::new(&r.value);
            let (label, value) = if r.lead { (label.strong(), value.strong()) } else { (label, value) };
            let l = ui.label(label);
            let v = ui.label(value);
            if !r.hint.is_empty() {
                l.on_hover_text(r.hint);
                v.on_hover_text(r.hint);
            }
            ui.end_row();
        }
    });
}

impl App {
    /// The full card for one player. Missing data shows as a short status line per section.
    pub(crate) fn detail_card(&mut self, ui: &mut egui::Ui, puuid: &str) {
        ui.set_width(CARD_WIDTH);
        let Some(PlayerInfo { riot_id, rank, stats, impact, impact_depth, progress }) = self.players.get(puuid).cloned()
        else {
            ui.label(RichText::new("No data for this player yet").weak());
            return;
        };
        let name = riot_id.unwrap_or_else(|| "…".into());
        ui.add(egui::Label::new(RichText::new(name).strong().size(16.0)).truncate());

        self.rank_section(ui, &rank);

        match &impact {
            Fetch::Ready(i) => self.impact_section(ui, i, impact_depth, progress.as_ref()),
            Fetch::Pending => {
                section(ui, "IMPACT");
                ui.label(RichText::new(progress_text(progress.as_ref(), 0)).weak());
            }
            Fetch::Unavailable(reason) => {
                section(ui, "IMPACT");
                ui.label(RichText::new(format!("Unavailable: {reason}")).weak());
            }
        }

        stats_section(ui, &stats);
        self.acts_section(ui, &rank);
    }

    fn rank_section(&self, ui: &mut egui::Ui, rank: &Fetch<RankProfile>) {
        section(ui, "RANK");
        let mut rows = vec![row("Current", self.rank_text(rank), "")];
        if let Fetch::Ready(profile) = rank
            && let Some((tier, act)) = self.peak(profile)
        {
            rows.push(row("Peak", format!("{} ({act})", self.tier_label(tier)), "Highest tier with a win, across all acts"));
        }
        rows_grid(ui, "rank_rows", &rows);
    }

    fn impact_section(&self, ui: &mut egui::Ui, i: &Impact, depth: Depth, progress: Option<&Progress>) {
        let scope = format!("{} MATCHES", depth.scope(i.matches, i.history_complete).to_ascii_uppercase());
        section(ui, &format!("IMPACT · {scope}"));
        let (label, color) = impact_label(i.rating);
        let agent = i.main_agent.as_deref();
        let assets = self.assets.as_deref();
        let role = agent.and_then(|a| assets?.agent_role(a));
        let agent_name = agent.and_then(|a| assets?.agent_name(a));
        egui::Sides::new().show(
            ui,
            |ui| {
                ui.label(RichText::new(format!("{:.2}", i.rating)).strong().size(20.0).color(color));
                ui.label(RichText::new(label).strong().color(color));
            },
            |ui| {
                if let (Some(role), Some(agent)) = (role, agent_name) {
                    ui.label(RichText::new(format!("{role} · {agent}")).weak());
                }
            },
        );
        ui.label(
            RichText::new(format!(
                "{:+.1}% round-win chance per round over {} rounds",
                i.wpa_per_round * 100.0,
                i.rounds
            ))
            .small()
            .weak(),
        );
        if !matches!(progress, None | Some(Progress::Done)) {
            ui.label(RichText::new(progress_text(progress, i.matches)).small().color(Color32::from_rgb(230, 170, 90)));
        }
        if !i.recent_rounds.is_empty() {
            ui.add_space(4.0);
            round_strip(ui, &i.recent_rounds);
            ui.label(RichText::new("Round by round, last 2 matches (hover a bar)").small().weak());
        }
        ui.add_space(4.0);

        // How much to trust the number, and against whom it was earned.
        let confidence = match i.reliability {
            r if r >= 0.75 => "high",
            r if r >= 0.5 => "medium",
            _ => "low",
        };
        let steadiness = match i.consistency_sd {
            s if s < 0.04 => "steady",
            s if s < 0.08 => "varies",
            _ => "streaky",
        };
        let mut context = vec![
            row(
                "Confidence",
                format!("{confidence} ({:.0}% own data)", i.reliability * 100.0),
                "Share of the rating that comes from this player's rounds rather than shrinkage toward break-even. \
                 Grows with rounds; falls when their rounds swing wildly",
            ),
            row(
                "Consistency",
                format!("{steadiness} (match-to-match ±{:.1}%)", i.consistency_sd * 100.0),
                "Spread of per-match impact. Streaky players carry some games and sink others",
            ),
            row(
                "Attack / defence",
                format!(
                    "{:+.1}% / {:+.1}% per round",
                    i.attack_wpa * 100.0,
                    i.defence_wpa * 100.0
                ),
                "Impact per round on each side",
            ),
        ];
        if i.opponent_tier > 0.0 {
            let gap = match i.tier_gap {
                g if g <= -1.0 => format!(" · punching up {:.1} tiers", -g),
                g if g >= 1.0 => format!(" · {:.1} tiers above them", g),
                _ => String::new(),
            };
            context.push(row(
                "Lobby strength",
                format!("avg opponent {}{gap}", self.tier_label(i.opponent_tier.round() as u32)),
                "Average competitive tier of opponents in these matches, from their rank at the time",
            ));
        }
        rows_grid(ui, "context_rows", &context);
        ui.add_space(6.0);

        let general = vec![
            row(
                "Opening duels",
                format!("{} won · {} lost", i.opening_wins, i.opening_losses),
                "First kill of the round: taken or given up",
            ),
            row("KAST", format!("{:.0}%", i.kast * 100.0), "Rounds with a kill, assist, survival or traded death"),
            row("Clutches", format!("{} won of {}", i.clutches_won, i.clutches), "Rounds left as the last player alive against two or more"),
            row("Impact frags", i.impact_frags.to_string(), "Kills after the opening duel that swung the round by 25+ points"),
        ];
        let mut trading = vec![
            row(
                "Trades",
                format!("{} instant · {} late", i.trades_fast, i.trades_late),
                "Killing a teammate's killer within 2 s (instant) or 2–5 s (late)",
            ),
            row(
                "Deaths traded",
                format!("{} of {} ({:.0}%)", i.deaths_traded, i.deaths, percent(i.deaths_traded, i.deaths)),
                "Deaths avenged by a teammate within 5 s",
            ),
            row(
                "Missed refrags",
                i.missed_refrags.to_string(),
                "A teammate died within ~15 m and their killer survived the next 5 s. Shown only; walls aren't known",
            ),
        ];
        let mut utility = vec![
            row("Util assists", i.util_assists.to_string(), "Assists on players they never damaged that round: flashes, debuffs"),
            row(
                "Casts per round",
                match i.agent_casts_per_round {
                    Some(avg) => format!("{:.1} (agent avg {avg:.1})", i.casts_per_round),
                    None => format!("{:.1}", i.casts_per_round),
                },
                "Ability casts per round from match totals. Usage alone isn't rated",
            ),
        ];
        if i.ability_kills > 0 {
            utility.push(row("Ability kills", i.ability_kills.to_string(), ""));
        }
        let mut entries = Vec::new();
        if i.entry_wins + i.entry_losses > 0 {
            entries.push(row(
                "Site entries",
                format!(
                    "{} won · {} lost ({:.0}%)",
                    i.entry_wins,
                    i.entry_losses,
                    percent(i.entry_wins, i.entry_wins + i.entry_losses)
                ),
                "The attackers' first fight inside a site before the plant",
            ));
            if let Some(spot) = &i.entry_spot {
                entries.push(row("Usual entry", spot.clone(), "Most common callout of their entry kills"));
            }
        }
        let mut lurks = Vec::new();
        if i.lurk_rounds > 0 {
            lurks.push(row(
                "Lurks",
                format!("{} · avg swing {:+.1}%", plural(i.lurk_rounds, "round", "rounds"), i.lurk_swing * 100.0),
                "Attack rounds where their own fight (or a solo plant) happened ~30 m+ from every teammate",
            ));
            lurks.push(row(
                "Lurk outcomes",
                format!(
                    "{} · {}",
                    plural(i.lurk_late_kills, "late kill", "late kills"),
                    plural(i.lurk_deaths_untraded, "untraded death", "untraded deaths")
                ),
                "Late: isolated kills after the round was already decided",
            ));
        }

        // The rows that matter most for the role go first, in bold.
        let lead = match role {
            Some("Duelist") => &mut entries,
            Some("Sentinel") => &mut lurks,
            Some("Initiator" | "Controller") => &mut utility,
            _ => &mut trading,
        };
        for r in lead.iter_mut() {
            r.lead = true;
        }
        let lead = std::mem::take(lead);
        let mut rows = lead;
        rows.extend(general);
        rows.extend(trading);
        rows.extend(utility);
        rows.extend(entries);
        rows.extend(lurks);
        if i.kills > 0 {
            rows.push(row(
                "Low-impact kills",
                format!("{} of {} ({:.0}%)", i.low_impact_kills, i.kills, percent(i.low_impact_kills, i.kills)),
                "Kills that moved round-win odds by under 3 points: exit frags, eco kills",
            ));
        }
        rows_grid(ui, "impact_rows", &rows);

        if let Some(note) = impact_note(i) {
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(note).italics().color(color)).wrap());
        }
    }

    fn acts_section(&self, ui: &mut egui::Ui, rank: &Fetch<RankProfile>) {
        let Fetch::Ready(profile) = rank else { return };
        let acts = self.recent_acts(profile, 4);
        if acts.is_empty() {
            return;
        }
        section(ui, "RECENT ACTS");
        egui::Grid::new("act_rows").num_columns(3).spacing([14.0, 3.0]).show(ui, |ui| {
            for (label, tier, wins, games) in acts {
                ui.label(RichText::new(label).color(Color32::from_gray(160)));
                ui.label(self.tier_label(tier));
                ui.label(RichText::new(format!("{wins} won of {games}")).weak());
                ui.end_row();
            }
        });
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
            .map(|(tier, a)| (tier, self.act_label(&a.act)))
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
            .map(|a| (self.act_label(&a.act), assets.normalize_tier(&a.act, a.tier), a.wins, a.games))
            .collect()
    }

    fn act_label(&self, act: &str) -> String {
        self.assets.as_ref().and_then(|a| a.acts.get(act)).map_or_else(|| "Unknown act".into(), |a| a.label.clone())
    }
}

/// One line describing where a player's deep fetch stands.
fn progress_text(progress: Option<&Progress>, have_now: u32) -> String {
    match progress {
        None => "Queued for match history…".into(),
        Some(Progress::Done) => String::new(),
        Some(Progress::Fetching { have, target }) => {
            let have = (*have).max(have_now);
            format!("Fetching match history… {have} so far, target: {}", target.label().to_lowercase())
        }
        Some(Progress::RateLimited { have, target, resume_secs }) => {
            let have = (*have).max(have_now);
            format!(
                "Waiting {resume_secs}s for API quota ({have} so far, target: {})",
                target.label().to_lowercase()
            )
        }
    }
}

fn stats_section(ui: &mut egui::Ui, stats: &Fetch<Stats>) {
    match stats {
        Fetch::Ready(s) => {
            section(ui, &format!("LAST {} COMPETITIVE", s.matches));
            let rows = [
                row("Win rate", format!("{:.0}% ({} won · {} lost)", percent(s.wins, s.wins + s.losses), s.wins, s.losses), ""),
                row("ACS / ADR", format!("{:.0} / {:.0}", s.acs, s.adr), "Average combat score and damage per round"),
                row("K/D · HS%", format!("{:.2} · {:.0}%", s.kd, s.hs_pct), ""),
            ];
            rows_grid(ui, "stats_rows", &rows);
        }
        Fetch::Pending => {
            section(ui, "RECENT COMPETITIVE");
            ui.label(RichText::new("Loading match stats…").weak());
        }
        Fetch::Unavailable(reason) => {
            section(ui, "RECENT COMPETITIVE");
            ui.label(RichText::new(format!("Unavailable: {reason}")).weak());
        }
    }
}

/// One bar per round (oldest left): up = round-win chance added, down = lost. A tick under each
/// bar marks rounds the team won; a gap separates matches. Hover for the round.
fn round_strip(ui: &mut egui::Ui, rounds: &[RoundSwing]) {
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
        let mut parts = vec![format!("Round {n}: {:+.1}%", r.swing * 100.0), plural(r.kills.into(), "kill", "kills")];
        for (flag, label) in [(r.died, "died"), (r.planted, "planted"), (r.defused, "defused")] {
            if flag {
                parts.push(label.into());
            }
        }
        parts.push((if r.won { "won" } else { "lost" }).into());
        response.on_hover_text_at_pointer(parts.join(" · "));
    }
}

/// Calls out where the scoreboard and the impact disagree.
fn impact_note(i: &Impact) -> Option<&'static str> {
    let kd = i.kills as f32 / i.deaths.max(1) as f32;
    let low_share = i.low_impact_kills as f32 / i.kills.max(1) as f32;
    if kd >= 1.10 && i.rating < 0.95 {
        Some("Kills aren't turning into rounds won.")
    } else if kd < 0.95 && i.rating >= 1.07 {
        Some("Wins rounds beyond what their K/D shows.")
    } else if i.kills >= 10 && low_share >= 0.40 {
        Some("Many kills come after the round is decided or against weaker buys.")
    } else {
        None
    }
}

