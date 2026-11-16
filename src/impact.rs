//! Impact rating: how much a player moves their team's chance of winning rounds.
//!
//! Each round of a full HenrikDev v4 match is replayed as a sequence of states
//! (attackers alive, defenders alive, spike planted, economy edge). A [`WinModel`] gives the
//! attackers' round-win probability for every state; each event is credited with the change it
//! caused (win-probability added, WPA):
//!
//! * kill: killer +75% of the swing (100% unassisted), assisters split 25%, victim −100%
//! * plant: planter +swing, alive defenders split −swing
//! * defuse: defuser +remaining probability, alive attackers split −
//! * round end (detonation, time, …): the residual is split among players still alive
//!
//! Every round is zero-sum between the teams. Kills that barely move the odds (exit frags, eco
//! kills against a far better-equipped side) earn little, which is what separates impact from K/D.
//!
//! The model learns from every match fetched (persisted to `winmodel.json`), shrinking sparse
//! states toward simple alive-count odds until real outcomes accumulate.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::Impact;

/// A kill within this long after a teammate's death, on that teammate's killer, is a trade.
const TRADE_WINDOW_MS: u64 = 5_000;
/// Kills moving round-win probability by less than this count as low-impact.
const LOW_IMPACT: f64 = 0.03;
/// Team loadout difference (credits) that counts as an economy edge.
const ECON_EDGE: i64 = 6_000;
const KILLER_SHARE_WITH_ASSISTS: f64 = 0.75;
/// Pseudo-rounds of zero impact added to every sample, so a few lucky rounds don't dominate.
const SHRINK_ROUNDS: f64 = 40.0;
/// Pseudo-observations pulling sparse states toward their parent estimate.
const CELL_PRIOR: f64 = 8.0;
/// Match ids remembered to avoid counting a shared match twice.
const SEEN_CAP: usize = 5_000;
/// Rating scale: before data accumulates, assume players' WPA/round spreads with this SD.
const SPREAD_PRIOR_SD: f64 = 0.03;
const SPREAD_PRIOR_N: f64 = 20.0;
/// Rating points per standard deviation of WPA/round.
const RATING_PER_SD: f64 = 0.15;

// ---- HenrikDev v4 match: only the fields the rating needs ----------------------------------

#[derive(Deserialize)]
pub struct MatchDto {
    metadata: MetaDto,
    #[serde(default)]
    rounds: Vec<RoundDto>,
    #[serde(default)]
    kills: Vec<KillDto>,
}

#[derive(Deserialize)]
struct MetaDto {
    match_id: String,
}

#[derive(Deserialize)]
struct RoundDto {
    winning_team: Option<String>,
    /// "Attacker" / "Defender".
    winning_team_role: Option<String>,
    plant: Option<SpikeDto>,
    defuse: Option<SpikeDto>,
    #[serde(default)]
    stats: Vec<RoundStatDto>,
}

#[derive(Deserialize)]
struct SpikeDto {
    round_time_in_ms: u64,
    player: PlayerRef,
}

#[derive(Deserialize)]
struct RoundStatDto {
    player: PlayerRef,
    #[serde(default)]
    economy: EconomyDto,
    #[serde(default)]
    was_afk: bool,
}

#[derive(Deserialize, Default)]
struct EconomyDto {
    #[serde(default)]
    loadout_value: i64,
}

#[derive(Deserialize)]
struct KillDto {
    /// 0-based index into `rounds`.
    round: usize,
    time_in_round_in_ms: u64,
    killer: PlayerRef,
    victim: PlayerRef,
    #[serde(default)]
    assistants: Vec<PlayerRef>,
}

#[derive(Deserialize)]
struct PlayerRef {
    puuid: String,
    #[serde(default)]
    team: String,
}

// ---- Round replay ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
struct State {
    a: u8,
    d: u8,
    planted: bool,
    /// +1 attackers better equipped, −1 defenders, 0 even.
    econ: i8,
}

impl State {
    /// Decided states have a fixed outcome and are never learned.
    fn decided(self) -> Option<f64> {
        if self.d == 0 {
            Some(1.0)
        } else if self.a == 0 && !self.planted {
            Some(0.0)
        } else {
            None
        }
    }
}

enum Event<'a> {
    Kill { killer: &'a str, victim: &'a str, assists: Vec<&'a str> },
    Plant(&'a str),
    Defuse(&'a str),
}

struct RoundCtx<'a> {
    /// true = attacker. Includes AFK players so their (non-)deaths can be ignored.
    side: HashMap<&'a str, bool>,
    start_att: Vec<&'a str>,
    start_def: Vec<&'a str>,
    econ: i8,
    attackers_won: bool,
    events: Vec<(u64, Event<'a>)>,
}

enum StepKind<'a> {
    Kill { killer: &'a str, victim: &'a str, assists: Vec<&'a str> },
    Plant(&'a str),
    Defuse(&'a str),
    /// A dead player died again (Sage revive); the state change is credited to nobody.
    Revive,
}

struct Step<'a> {
    t: u64,
    kind: StepKind<'a>,
    after: State,
    alive_att: Vec<&'a str>,
    alive_def: Vec<&'a str>,
}

/// Rounds with known sides, ready to replay. Rounds missing a winner or role are skipped.
fn rounds(m: &MatchDto) -> Vec<RoundCtx<'_>> {
    let mut kills_by_round: HashMap<usize, Vec<&KillDto>> = HashMap::new();
    for k in &m.kills {
        kills_by_round.entry(k.round).or_default().push(k);
    }
    let mut out = Vec::with_capacity(m.rounds.len());
    for (i, r) in m.rounds.iter().enumerate() {
        let (Some(winner), Some(role)) = (r.winning_team.as_deref(), r.winning_team_role.as_deref()) else {
            continue;
        };
        let attacking_team = if role.starts_with("Att") {
            winner
        } else if let Some(other) = r.stats.iter().map(|s| s.player.team.as_str()).find(|t| *t != winner) {
            other
        } else {
            continue;
        };
        let mut ctx = RoundCtx {
            side: HashMap::new(),
            start_att: Vec::new(),
            start_def: Vec::new(),
            econ: 0,
            attackers_won: winner == attacking_team,
            events: Vec::new(),
        };
        let mut econ_diff = 0i64;
        for s in &r.stats {
            let attacker = s.player.team == attacking_team;
            ctx.side.insert(&s.player.puuid, attacker);
            econ_diff += if attacker { s.economy.loadout_value } else { -s.economy.loadout_value };
            if !s.was_afk {
                if attacker { &mut ctx.start_att } else { &mut ctx.start_def }.push(&s.player.puuid);
            }
        }
        if ctx.start_att.is_empty() || ctx.start_def.is_empty() {
            continue;
        }
        ctx.econ = if econ_diff > ECON_EDGE {
            1
        } else if econ_diff < -ECON_EDGE {
            -1
        } else {
            0
        };
        for k in kills_by_round.get(&i).into_iter().flatten() {
            let assists = k.assistants.iter().map(|a| a.puuid.as_str()).collect();
            let ev = Event::Kill { killer: &k.killer.puuid, victim: &k.victim.puuid, assists };
            ctx.events.push((k.time_in_round_in_ms, ev));
        }
        if let Some(p) = &r.plant {
            ctx.events.push((p.round_time_in_ms, Event::Plant(&p.player.puuid)));
        }
        if let Some(d) = &r.defuse {
            ctx.events.push((d.round_time_in_ms, Event::Defuse(&d.player.puuid)));
        }
        ctx.events.sort_by_key(|(t, _)| *t);
        out.push(ctx);
    }
    out
}

fn replay<'a>(r: &RoundCtx<'a>) -> (State, Vec<Step<'a>>) {
    let mut att = r.start_att.clone();
    let mut def = r.start_def.clone();
    let mut dead: HashSet<&str> = HashSet::new();
    let mut planted = false;
    let state = |att: &[&str], def: &[&str], planted| State {
        a: att.len() as u8,
        d: def.len() as u8,
        planted,
        econ: r.econ,
    };
    let start = state(&att, &def, false);
    let mut steps = Vec::with_capacity(r.events.len());
    for (t, ev) in &r.events {
        let t = *t;
        match ev {
            Event::Kill { killer, victim, assists } => {
                let Some(&victim_att) = r.side.get(victim) else { continue };
                let alive_on_side = if victim_att { att.contains(victim) } else { def.contains(victim) };
                if !alive_on_side {
                    if !dead.remove(victim) {
                        continue; // AFK player: not part of the fight
                    }
                    if victim_att { &mut att } else { &mut def }.push(victim);
                    let after = state(&att, &def, planted);
                    steps.push(Step { t, kind: StepKind::Revive, after, alive_att: att.clone(), alive_def: def.clone() });
                }
                if victim_att { &mut att } else { &mut def }.retain(|p| p != victim);
                dead.insert(victim);
                let kind = StepKind::Kill { killer, victim, assists: assists.clone() };
                let after = state(&att, &def, planted);
                steps.push(Step { t, kind, after, alive_att: att.clone(), alive_def: def.clone() });
            }
            Event::Plant(planter) => {
                planted = true;
                let after = state(&att, &def, planted);
                steps.push(Step { t, kind: StepKind::Plant(planter), after, alive_att: att.clone(), alive_def: def.clone() });
            }
            Event::Defuse(defuser) => {
                let after = state(&att, &def, planted);
                steps.push(Step { t, kind: StepKind::Defuse(defuser), after, alive_att: att.clone(), alive_def: def.clone() });
                break;
            }
        }
    }
    (start, steps)
}

// ---- Win-probability model ------------------------------------------------------------------

/// Attackers' round-win probability per state, learned from fetched matches.
#[derive(Default)]
pub struct WinModel {
    /// State → [rounds that passed through it, attacker wins].
    cells: HashMap<State, [u32; 2]>,
    seen: VecDeque<String>,
    seen_set: HashSet<String>,
    /// Count and sum of squares of per-player WPA/round, for the rating scale.
    spread_n: f64,
    spread_ss: f64,
}

#[derive(Serialize, Deserialize, Default)]
struct Stored {
    cells: Vec<(State, [u32; 2])>,
    seen: Vec<String>,
    spread_n: f64,
    spread_ss: f64,
}

/// Alive-count odds used until a state has data. A planted spike favours attackers.
fn prior(s: State) -> f64 {
    if s.a == 0 {
        return 0.2; // planted, attackers dead: defenders still have to defuse
    }
    let base = s.a as f64 / (s.a + s.d) as f64;
    if s.planted { (base + 0.2).min(0.95) } else { base }
}

impl WinModel {
    fn path() -> Option<PathBuf> {
        crate::config::local_data_dir().map(|d| d.join("winmodel.json"))
    }

    /// Missing or corrupt files start a fresh model.
    pub fn load() -> Self {
        let stored: Stored = Self::path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            cells: stored.cells.into_iter().collect(),
            seen_set: stored.seen.iter().cloned().collect(),
            seen: stored.seen.into(),
            spread_n: stored.spread_n,
            spread_ss: stored.spread_ss,
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path().ok_or_else(|| std::io::Error::other("no local data directory"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let stored = Stored {
            cells: self.cells.iter().map(|(k, v)| (*k, *v)).collect(),
            seen: self.seen.iter().cloned().collect(),
            spread_n: self.spread_n,
            spread_ss: self.spread_ss,
        };
        std::fs::write(path, serde_json::to_vec(&stored)?)
    }

    fn p_attack(&self, s: State) -> f64 {
        if let Some(p) = s.decided() {
            return p;
        }
        // Economy-specific estimate, shrunk toward the economy-pooled estimate, shrunk toward the prior.
        let (mut n, mut w) = (0.0, 0.0);
        for econ in -1..=1 {
            if let Some([cn, cw]) = self.cells.get(&State { econ, ..s }) {
                n += *cn as f64;
                w += *cw as f64;
            }
        }
        let pooled = (w + CELL_PRIOR * prior(s)) / (n + CELL_PRIOR);
        let [n, w] = self.cells.get(&s).copied().unwrap_or([0, 0]);
        (w as f64 + CELL_PRIOR * pooled) / (n as f64 + CELL_PRIOR)
    }

    /// Learns from matches not seen before. Returns whether anything was added.
    pub fn ingest(&mut self, matches: &[MatchDto]) -> bool {
        let mut changed = false;
        for m in matches {
            if !self.seen_set.insert(m.metadata.match_id.clone()) {
                continue;
            }
            self.seen.push_back(m.metadata.match_id.clone());
            if self.seen.len() > SEEN_CAP
                && let Some(old) = self.seen.pop_front()
            {
                self.seen_set.remove(&old);
            }
            for round in rounds(m) {
                let (start, steps) = replay(&round);
                let visited: HashSet<State> = std::iter::once(start)
                    .chain(steps.iter().filter(|s| !matches!(s.kind, StepKind::Defuse(_))).map(|s| s.after))
                    .filter(|s| s.decided().is_none())
                    .collect();
                for s in visited {
                    let cell = self.cells.entry(s).or_insert([0, 0]);
                    cell[0] += 1;
                    cell[1] += round.attackers_won as u32;
                }
                changed = true;
            }
        }
        changed
    }

    /// Maps WPA/round to the 1.00-centred rating and records it to refine the scale.
    fn rate(&mut self, wpa_per_round: f64) -> f64 {
        self.spread_n += 1.0;
        self.spread_ss += wpa_per_round * wpa_per_round;
        let sd = ((self.spread_ss + SPREAD_PRIOR_N * SPREAD_PRIOR_SD.powi(2)) / (self.spread_n + SPREAD_PRIOR_N)).sqrt();
        1.0 + RATING_PER_SD * wpa_per_round / sd
    }
}

// ---- Crediting ------------------------------------------------------------------------------

struct KillRecord<'a> {
    t: u64,
    killer: &'a str,
    victim: &'a str,
    assists: Vec<&'a str>,
    /// Round-win probability gained by the killer's team.
    gain: f64,
}

struct RoundCredit<'a> {
    wpa: HashMap<&'a str, f64>,
    kills: Vec<KillRecord<'a>>,
    survivors: Vec<&'a str>,
    /// Players who were their side's last one alive against two or more.
    clutchers: HashSet<&'a str>,
}

fn split<'a>(wpa: &mut HashMap<&'a str, f64>, players: &[&'a str], amount: f64) {
    if players.is_empty() {
        return;
    }
    let each = amount / players.len() as f64;
    for p in players {
        *wpa.entry(*p).or_default() += each;
    }
}

fn credit_round<'a>(r: &RoundCtx<'a>, model: &WinModel) -> RoundCredit<'a> {
    let (start, steps) = replay(r);
    let mut wpa: HashMap<&str, f64> = HashMap::new();
    let mut kills = Vec::new();
    let mut clutchers = HashSet::new();
    let mut p = model.p_attack(start);
    let (mut alive_att, mut alive_def) = (r.start_att.clone(), r.start_def.clone());

    for step in steps {
        let p_after = match step.kind {
            StepKind::Defuse(_) => 0.0,
            _ => model.p_attack(step.after),
        };
        let swing = p_after - p; // attackers' perspective
        match &step.kind {
            StepKind::Kill { killer, victim, assists } => {
                let victim_att = r.side[victim];
                let killer_side = r.side.get(killer).copied();
                // Gain for the side opposing the victim. A thinly sampled state can make a kill look
                // harmful to the killer's side; never credit that as negative.
                let gain = (if victim_att { -swing } else { swing }).max(0.0);
                *wpa.entry(victim).or_default() -= gain;
                if killer_side == Some(!victim_att) {
                    let helpers: Vec<&str> = assists
                        .iter()
                        .copied()
                        .filter(|a| a != killer && r.side.get(a) == Some(&!victim_att))
                        .collect();
                    let share = if helpers.is_empty() { 1.0 } else { KILLER_SHARE_WITH_ASSISTS };
                    *wpa.entry(killer).or_default() += gain * share;
                    split(&mut wpa, &helpers, gain * (1.0 - share));
                    kills.push(KillRecord { t: step.t, killer, victim, assists: helpers, gain });
                } else {
                    // Team kill / spike / fall: the opposing side's survivors take the gain.
                    let opponents = if victim_att { &step.alive_def } else { &step.alive_att };
                    split(&mut wpa, opponents, gain);
                }
            }
            StepKind::Plant(planter) => {
                let gain = swing.max(0.0);
                *wpa.entry(planter).or_default() += gain;
                split(&mut wpa, &step.alive_def, -gain);
            }
            StepKind::Defuse(defuser) => {
                *wpa.entry(defuser).or_default() -= swing;
                split(&mut wpa, or_lineup(&step.alive_att, &r.start_att), swing);
            }
            StepKind::Revive => {}
        }
        p = p_after;
        alive_att = step.alive_att;
        alive_def = step.alive_def;
        for (mine, theirs) in [(&alive_att, &alive_def), (&alive_def, &alive_att)] {
            if mine.len() == 1 && theirs.len() >= 2 {
                clutchers.insert(mine[0]);
            }
        }
    }

    // Whatever the events didn't explain (detonation, time, defuse aftermath) goes to the living;
    // a wiped side's share goes to its whole lineup so the round still nets to zero.
    let residual = if r.attackers_won { 1.0 } else { 0.0 } - p;
    split(&mut wpa, or_lineup(&alive_att, &r.start_att), residual);
    split(&mut wpa, or_lineup(&alive_def, &r.start_def), -residual);
    let survivors = alive_att.into_iter().chain(alive_def).collect();
    RoundCredit { wpa, kills, survivors, clutchers }
}

fn or_lineup<'s, 'a>(alive: &'s [&'a str], lineup: &'s [&'a str]) -> &'s [&'a str] {
    if alive.is_empty() { lineup } else { alive }
}

/// Impact for `puuid` across `matches`. Also refines the model's rating scale.
/// `None` when the player has no playable rounds in the sample.
pub fn evaluate(matches: &[MatchDto], puuid: &str, model: &mut WinModel) -> Option<Impact> {
    let mut wpa = 0.0;
    let mut out = Impact {
        rating: 1.0,
        wpa_per_round: 0.0,
        rounds: 0,
        matches: 0,
        kills: 0,
        deaths: 0,
        opening_wins: 0,
        opening_losses: 0,
        kast: 0.0,
        trade_kills: 0,
        deaths_traded: 0,
        clutches: 0,
        clutches_won: 0,
        low_impact_kills: 0,
    };
    let mut kast_rounds = 0u32;

    for m in matches {
        let mut played = false;
        for r in rounds(m) {
            let Some(&attacker) = r.side.get(puuid) else { continue };
            if !r.start_att.contains(&puuid) && !r.start_def.contains(&puuid) {
                continue; // AFK this round
            }
            played = true;
            out.rounds += 1;
            let c = credit_round(&r, model);
            wpa += c.wpa.get(puuid).copied().unwrap_or(0.0);

            let mut kast = c.survivors.contains(&puuid);
            if let Some(first) = c.kills.first() {
                out.opening_wins += (first.killer == puuid) as u32;
                out.opening_losses += (first.victim == puuid) as u32;
            }
            for (i, k) in c.kills.iter().enumerate() {
                // Earlier kills by this kill's victim, on the killer's teammates, inside the window.
                let avenged = c.kills[..i]
                    .iter()
                    .filter(|e| e.killer == k.victim && k.t - e.t <= TRADE_WINDOW_MS)
                    .map(|e| e.victim)
                    .collect::<Vec<_>>();
                if k.killer == puuid {
                    out.kills += 1;
                    kast = true;
                    out.low_impact_kills += (k.gain < LOW_IMPACT) as u32;
                    out.trade_kills += (!avenged.is_empty()) as u32;
                }
                if k.assists.contains(&puuid) {
                    kast = true;
                }
                if avenged.contains(&puuid) {
                    out.deaths_traded += 1;
                    kast = true;
                }
            }
            out.deaths += c.kills.iter().filter(|k| k.victim == puuid).count() as u32;
            kast_rounds += kast as u32;
            if c.clutchers.contains(puuid) {
                out.clutches += 1;
                out.clutches_won += (attacker == r.attackers_won) as u32;
            }
        }
        out.matches += played as u32;
    }
    if out.rounds == 0 {
        return None;
    }
    let shrunk = wpa / (out.rounds as f64 + SHRINK_ROUNDS);
    out.wpa_per_round = shrunk as f32;
    out.rating = model.rate(shrunk) as f32;
    out.kast = kast_rounds as f32 / out.rounds as f32;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round 0: attackers (Red: a1..a5) win after a plant; a1 opens, d1 trades a1, a2 trades d1,
    /// a2 is left 1v2 and wins. Round 1: defenders defuse.
    fn sample() -> MatchDto {
        let stat = |p: &str, team: &str| {
            format!(r#"{{"player":{{"puuid":"{p}","team":"{team}"}},"economy":{{"loadout_value":3900}}}}"#)
        };
        let stats: Vec<String> = (1..=5)
            .map(|i| stat(&format!("a{i}"), "Red"))
            .chain((1..=5).map(|i| stat(&format!("d{i}"), "Blue")))
            .collect();
        let stats = stats.join(",");
        let kill = |round: u32, t: u32, killer: &str, kt: &str, victim: &str, vt: &str, assist: &str| {
            let assists = if assist.is_empty() {
                String::new()
            } else {
                format!(r#"{{"puuid":"{assist}","team":"{kt}"}}"#)
            };
            format!(
                r#"{{"round":{round},"time_in_round_in_ms":{t},"killer":{{"puuid":"{killer}","team":"{kt}"}},
                   "victim":{{"puuid":"{victim}","team":"{vt}"}},"assistants":[{assists}]}}"#
            )
        };
        let kills = [
            kill(0, 10_000, "a1", "Red", "d1", "Blue", ""),
            kill(0, 12_000, "d2", "Blue", "a1", "Red", "d3"),
            kill(0, 13_000, "a2", "Red", "d2", "Blue", ""),
            kill(0, 20_000, "d3", "Blue", "a3", "Red", ""),
            kill(0, 21_000, "d3", "Blue", "a4", "Red", ""),
            kill(0, 22_000, "d4", "Blue", "a5", "Red", ""),
            kill(0, 40_000, "a2", "Red", "d3", "Blue", ""),
            kill(0, 41_000, "a2", "Red", "d4", "Blue", ""),
            kill(1, 30_000, "d1", "Blue", "a1", "Red", ""),
        ]
        .join(",");
        let json = format!(
            r#"{{"metadata":{{"match_id":"m1"}},
               "rounds":[
                 {{"winning_team":"Red","winning_team_role":"Attacker",
                   "plant":{{"round_time_in_ms":30000,"player":{{"puuid":"a2","team":"Red"}}}},
                   "stats":[{stats}]}},
                 {{"winning_team":"Blue","winning_team_role":"Defender",
                   "plant":{{"round_time_in_ms":25000,"player":{{"puuid":"a2","team":"Red"}}}},
                   "defuse":{{"round_time_in_ms":60000,"player":{{"puuid":"d5","team":"Blue"}}}},
                   "stats":[{stats}]}}
               ],
               "kills":[{kills}]}}"#
        );
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn every_round_is_zero_sum() {
        let m = sample();
        let model = WinModel::default();
        for r in rounds(&m) {
            let total: f64 = credit_round(&r, &model).wpa.values().sum();
            assert!(total.abs() < 1e-9, "round credit must net to zero, got {total}");
        }
    }

    #[test]
    fn credits_openings_trades_and_clutches() {
        let m = sample();
        let mut model = WinModel::default();
        let a2 = evaluate(std::slice::from_ref(&m), "a2", &mut model).unwrap();
        assert_eq!((a2.rounds, a2.kills, a2.trade_kills), (2, 3, 1));
        assert_eq!((a2.clutches, a2.clutches_won), (1, 1), "a2 won a 1v2");
        assert!(a2.rating > 1.0, "clutching a won round is positive impact");

        let a1 = evaluate(std::slice::from_ref(&m), "a1", &mut model).unwrap();
        assert_eq!((a1.opening_wins, a1.opening_losses), (1, 1));
        assert_eq!(a1.deaths_traded, 1, "a2 traded a1's death");

        let d5 = evaluate(std::slice::from_ref(&m), "d5", &mut model).unwrap();
        assert!(d5.wpa_per_round > 0.0, "the defuse is credited even without kills");
    }

    #[test]
    fn model_learns_each_match_once() {
        let m = sample();
        let mut model = WinModel::default();
        let start = State { a: 5, d: 5, planted: false, econ: 0 };
        assert!((model.p_attack(start) - 0.5).abs() < 1e-9);
        assert!(model.ingest(std::slice::from_ref(&m)));
        assert!(!model.ingest(std::slice::from_ref(&m)), "same match id is ignored");
        assert_eq!(model.cells[&start], [2, 1]);
        assert_eq!(model.p_attack(State { d: 0, ..start }), 1.0);
        assert_eq!(model.p_attack(State { a: 0, ..start }), 0.0);
    }
}
