//! Impact rating: how much a player moves their team's chance of winning rounds, in the style of
//! HLTV Rating 3.0's Round Swing, computed from full HenrikDev v4 match timelines.
//!
//! Each round is replayed as a sequence of states: attackers alive, defenders alive, spike
//! planted, economy edge, and time left (round clock before the plant, spike timer after). A
//! [`WinModel`] gives the attackers' round-win probability per state, and every event is credited
//! with the change it caused:
//!
//! * kill: the swing is split 50% final blow, 35% by damage dealt to the victim that round, 15%
//!   to utility assisters (assists with no damage: flashes, debuffs); the victim loses all of it.
//!   A death traded within 3 s gets 40% of its loss back from the trader.
//! * plant: planter gains the swing, alive defenders share the loss.
//! * defuse: defuser gains what the attackers had left, alive attackers share the loss.
//! * round end: whatever the events didn't explain goes to survivors.
//!
//! Swings that went to the side that ended up losing the round count half. Time-awareness makes
//! kills in decided rounds (no time to plant or defuse) worth little on its own. Every round nets
//! to zero between the teams.
//!
//! The model learns from every match fetched (persisted to `winmodel-v2.json`), shrinking sparse
//! states toward coarser estimates and finally toward simple alive-count odds.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{Impact, RoundSwing};
use crate::val_assets::Assets;

/// Combat phase length before the plant, and spike timer after it.
const ROUND_MS: u64 = 100_000;
const SPIKE_MS: u64 = 45_000;
/// A kill on a teammate's killer within this window is a trade (KAST, trade stats).
const TRADE_WINDOW_MS: u64 = 5_000;
const FAST_TRADE_MS: u64 = 2_000;
/// A death traded this quickly gets part of its loss back from the trader.
const REFUND_WINDOW_MS: u64 = 3_000;
const REFUND_SHARE: f64 = 0.4;
/// Swings in favour of the side that lost the round count this much.
const LOST_ROUND_WEIGHT: f64 = 0.5;
const FINAL_BLOW: f64 = 0.50;
const DAMAGE_SHARE: f64 = 0.35;
const UTIL_SHARE: f64 = 0.15;
const LOW_IMPACT: f64 = 0.03;
/// An even mid-round kill already swings ~20 points; this singles out kills from behind,
/// retakes and clutch-deciding kills.
const IMPACT_FRAG: f64 = 0.25;
/// Team loadout difference (credits) that counts as an economy edge.
const ECON_EDGE: i64 = 6_000;
/// Game units are centimetres: ~15 m counts as close enough to refrag, ~30 m from every
/// teammate (the top ~15% of nearest-teammate distances) as lurking.
const NEAR_UNITS: f32 = 1_500.0;
const LURK_UNITS: f32 = 3_000.0;
/// Matches shown in the round strip.
const STRIP_MATCHES: usize = 2;
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
    players: Vec<MatchPlayerDto>,
    #[serde(default)]
    rounds: Vec<RoundDto>,
    #[serde(default)]
    kills: Vec<KillDto>,
}

#[derive(Deserialize)]
struct MetaDto {
    match_id: String,
    #[serde(default)]
    map: Option<IdDto>,
}

#[derive(Deserialize)]
struct IdDto {
    id: String,
}

#[derive(Deserialize)]
struct MatchPlayerDto {
    puuid: String,
    #[serde(default)]
    agent: Option<IdDto>,
    #[serde(default)]
    ability_casts: CastsDto,
}

/// Whole-match totals; per-round counts are not populated by the API.
#[derive(Deserialize, Default)]
#[serde(default)]
struct CastsDto {
    grenade: Option<u32>,
    ability1: Option<u32>,
    ability2: Option<u32>,
    ultimate: Option<u32>,
}

impl CastsDto {
    fn total(&self) -> u32 {
        [self.grenade, self.ability1, self.ability2, self.ultimate].iter().flatten().sum()
    }
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
    /// Milliseconds since the combat phase began (the buy phase is excluded).
    round_time_in_ms: u64,
    player: PlayerRef,
    #[serde(default)]
    player_locations: Vec<LocatedDto>,
}

#[derive(Deserialize)]
struct RoundStatDto {
    player: PlayerRef,
    #[serde(default)]
    economy: EconomyDto,
    #[serde(default)]
    was_afk: bool,
    /// Damage this player dealt to each opponent during the round.
    #[serde(default)]
    damage_events: Vec<DamageDto>,
}

#[derive(Deserialize, Default)]
struct EconomyDto {
    #[serde(default)]
    loadout_value: i64,
}

#[derive(Deserialize)]
struct DamageDto {
    player: PlayerRef,
    #[serde(default)]
    damage: u32,
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
    /// Victim's position.
    #[serde(default)]
    location: Option<PointDto>,
    #[serde(default)]
    weapon: Option<WeaponDto>,
    /// Everyone alive at the moment of the kill except the victim.
    #[serde(default)]
    player_locations: Vec<LocatedDto>,
}

#[derive(Deserialize)]
struct WeaponDto {
    /// "Weapon" or "Ability".
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct LocatedDto {
    player: PlayerRef,
    location: PointDto,
}

#[derive(Deserialize, Clone, Copy)]
struct PointDto {
    x: f32,
    y: f32,
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
    /// 0 = most time left … 3 = least: round clock before the plant, spike timer after.
    time: u8,
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

fn time_bucket(t: u64, plant_t: Option<u64>) -> u8 {
    let (left, cuts) = match plant_t {
        None => (ROUND_MS.saturating_sub(t), [60_000, 30_000, 15_000]),
        // Under 7 s the defenders can no longer complete a fresh defuse.
        Some(p) => (SPIKE_MS.saturating_sub(t.saturating_sub(p)), [30_000, 15_000, 7_000]),
    };
    cuts.iter().take_while(|c| left <= **c).count() as u8
}

enum Event<'a> {
    Kill(&'a KillDto),
    Plant(&'a SpikeDto),
    Defuse(&'a SpikeDto),
}

struct RoundCtx<'a> {
    /// true = attacker. Includes AFK players so their (non-)deaths can be ignored.
    side: HashMap<&'a str, bool>,
    start_att: Vec<&'a str>,
    start_def: Vec<&'a str>,
    econ: i8,
    attackers_won: bool,
    events: Vec<(u64, Event<'a>)>,
    /// (dealer, target) → damage dealt during the round.
    damage: HashMap<(&'a str, &'a str), u32>,
    plant: Option<&'a SpikeDto>,
}

enum StepKind<'a> {
    Kill(&'a KillDto),
    Plant(&'a SpikeDto),
    Defuse(&'a SpikeDto),
    /// A dead player died again (Sage revive); the state change is credited to nobody.
    Revive,
}

struct Step<'a> {
    kind: StepKind<'a>,
    before: State,
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
            damage: HashMap::new(),
            plant: r.plant.as_ref(),
        };
        let mut econ_diff = 0i64;
        for s in &r.stats {
            let attacker = s.player.team == attacking_team;
            ctx.side.insert(&s.player.puuid, attacker);
            econ_diff += if attacker { s.economy.loadout_value } else { -s.economy.loadout_value };
            if !s.was_afk {
                if attacker { &mut ctx.start_att } else { &mut ctx.start_def }.push(&s.player.puuid);
            }
            for d in &s.damage_events {
                *ctx.damage.entry((&s.player.puuid, &d.player.puuid)).or_default() += d.damage;
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
            ctx.events.push((k.time_in_round_in_ms, Event::Kill(k)));
        }
        if let Some(p) = &r.plant {
            ctx.events.push((p.round_time_in_ms, Event::Plant(p)));
        }
        if let Some(d) = &r.defuse {
            ctx.events.push((d.round_time_in_ms, Event::Defuse(d)));
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
    let mut plant_t = None;
    let state = |att: &[&str], def: &[&str], t: u64, plant_t: Option<u64>| State {
        a: att.len() as u8,
        d: def.len() as u8,
        planted: plant_t.is_some(),
        econ: r.econ,
        time: time_bucket(t, plant_t),
    };
    let start = state(&att, &def, 0, None);
    let mut steps = Vec::with_capacity(r.events.len());
    for (t, ev) in &r.events {
        let t = *t;
        let before = state(&att, &def, t, plant_t);
        match ev {
            Event::Kill(k) => {
                let victim = k.victim.puuid.as_str();
                let Some(&victim_att) = r.side.get(victim) else { continue };
                let alive_on_side = if victim_att { att.contains(&victim) } else { def.contains(&victim) };
                let mut before = before;
                if !alive_on_side {
                    if !dead.remove(victim) {
                        continue; // AFK player: not part of the fight
                    }
                    if victim_att { &mut att } else { &mut def }.push(victim);
                    let after = state(&att, &def, t, plant_t);
                    let (alive_att, alive_def) = (att.clone(), def.clone());
                    steps.push(Step { kind: StepKind::Revive, before, after, alive_att, alive_def });
                    before = after;
                }
                if victim_att { &mut att } else { &mut def }.retain(|p| *p != victim);
                dead.insert(victim);
                let after = state(&att, &def, t, plant_t);
                let (alive_att, alive_def) = (att.clone(), def.clone());
                steps.push(Step { kind: StepKind::Kill(k), before, after, alive_att, alive_def });
            }
            Event::Plant(p) => {
                plant_t = Some(t);
                let after = state(&att, &def, t, plant_t);
                let (alive_att, alive_def) = (att.clone(), def.clone());
                steps.push(Step { kind: StepKind::Plant(p), before, after, alive_att, alive_def });
            }
            Event::Defuse(d) => {
                let (alive_att, alive_def) = (att.clone(), def.clone());
                steps.push(Step { kind: StepKind::Defuse(d), before, after: before, alive_att, alive_def });
                break;
            }
        }
    }
    (start, steps)
}

// ---- Win-probability model ------------------------------------------------------------------

/// Attackers' round-win probability per state, learned from fetched matches, plus per-agent
/// utility baselines.
#[derive(Default)]
pub struct WinModel {
    /// State → [rounds that passed through it, attacker wins].
    cells: HashMap<State, [u32; 2]>,
    seen: VecDeque<String>,
    seen_set: HashSet<String>,
    /// Count and sum of squares of per-player WPA/round, for the rating scale.
    spread_n: f64,
    spread_ss: f64,
    /// Agent UUID → [ability casts, rounds played].
    agent_util: HashMap<String, [f64; 2]>,
}

#[derive(Serialize, Deserialize, Default)]
struct Stored {
    cells: Vec<(State, [u32; 2])>,
    seen: Vec<String>,
    spread_n: f64,
    spread_ss: f64,
    agent_util: Vec<(String, [f64; 2])>,
}

/// Alive-count odds used until a state has data, adjusted for the spike and the clock.
fn prior(s: State) -> f64 {
    if s.a == 0 {
        return 0.2; // planted, attackers dead: defenders still have to defuse
    }
    let base = s.a as f64 / (s.a + s.d) as f64;
    if s.planted {
        match s.time {
            3 => base.max(0.9),
            2 => (base + 0.3).min(0.95),
            _ => (base + 0.2).min(0.95),
        }
    } else {
        // Late in the round attackers must still reach a site and plant.
        base * [1.0, 1.0, 0.85, 0.6][s.time as usize]
    }
}

impl WinModel {
    fn path() -> Option<PathBuf> {
        crate::config::local_data_dir().map(|d| d.join("winmodel-v2.json"))
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
            agent_util: stored.agent_util.into_iter().collect(),
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
            agent_util: self.agent_util.iter().map(|(k, v)| (k.clone(), *v)).collect(),
        };
        std::fs::write(path, serde_json::to_vec(&stored)?)
    }

    fn count(&self, s: State) -> (f64, f64) {
        self.cells.get(&s).map_or((0.0, 0.0), |[n, w]| (*n as f64, *w as f64))
    }

    /// Exact cell → pooled over economy → pooled over economy and time → prior.
    fn p_attack(&self, s: State) -> f64 {
        if let Some(p) = s.decided() {
            return p;
        }
        let shrink = |(n, w): (f64, f64), parent: f64| (w + CELL_PRIOR * parent) / (n + CELL_PRIOR);
        let sum = |states: &mut dyn Iterator<Item = State>| {
            states.map(|s| self.count(s)).fold((0.0, 0.0), |acc, (n, w)| (acc.0 + n, acc.1 + w))
        };
        let coarse = sum(&mut (-1..=1).flat_map(|econ| (0..4).map(move |time| State { econ, time, ..s })));
        let p = shrink(coarse, prior(s));
        let timed = sum(&mut (-1..=1).map(|econ| State { econ, ..s }));
        let p = shrink(timed, p);
        shrink(self.count(s), p)
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
            let played = rounds_played(m);
            for p in &m.players {
                let (Some(agent), Some(&n)) = (&p.agent, played.get(p.puuid.as_str())) else { continue };
                let entry = self.agent_util.entry(agent.id.to_ascii_lowercase()).or_default();
                entry[0] += p.ability_casts.total() as f64;
                entry[1] += n as f64;
            }
            for round in rounds(m) {
                let (start, steps) = replay(&round);
                let visited: HashSet<State> = std::iter::once(Some(start))
                    .chain(steps.iter().flat_map(|s| match s.kind {
                        StepKind::Defuse(_) => [Some(s.before), None],
                        _ => [Some(s.before), Some(s.after)],
                    }))
                    .flatten()
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

    /// The 1.00-centred rating for a WPA/round value.
    fn rating(&self, wpa_per_round: f64) -> f64 {
        let sd = ((self.spread_ss + SPREAD_PRIOR_N * SPREAD_PRIOR_SD.powi(2)) / (self.spread_n + SPREAD_PRIOR_N)).sqrt();
        1.0 + RATING_PER_SD * wpa_per_round / sd
    }

    /// Adds a player's final WPA/round to the spread that scales ratings.
    pub fn record_spread(&mut self, wpa_per_round: f64) {
        self.spread_n += 1.0;
        self.spread_ss += wpa_per_round * wpa_per_round;
    }

    fn agent_casts_per_round(&self, agent: &str) -> Option<f32> {
        let [casts, rounds] = self.agent_util.get(agent)?;
        (*rounds > 0.0).then(|| (casts / rounds) as f32)
    }
}

/// Rounds each player was present and not AFK.
fn rounds_played(m: &MatchDto) -> HashMap<&str, u32> {
    let mut played = HashMap::new();
    for r in &m.rounds {
        for s in r.stats.iter().filter(|s| !s.was_afk) {
            *played.entry(s.player.puuid.as_str()).or_default() += 1;
        }
    }
    played
}

// ---- Crediting ------------------------------------------------------------------------------

struct KillRecord<'a> {
    dto: &'a KillDto,
    /// Unweighted swing toward the killer's side (≥ 0).
    raw_gain: f64,
    /// Weighted loss charged to the victim.
    loss: f64,
    /// Same-side assisters other than the killer.
    assists: Vec<&'a str>,
    /// Assisters who dealt no damage to the victim (utility assists).
    util_assists: Vec<&'a str>,
}

impl KillRecord<'_> {
    fn t(&self) -> u64 {
        self.dto.time_in_round_in_ms
    }
    fn killer(&self) -> &str {
        &self.dto.killer.puuid
    }
    fn victim(&self) -> &str {
        &self.dto.victim.puuid
    }
}

struct RoundCredit<'a> {
    wpa: HashMap<&'a str, f64>,
    /// Cross-team kills in time order.
    kills: Vec<KillRecord<'a>>,
    survivors: Vec<&'a str>,
    /// Players who were their side's last one alive against two or more.
    clutchers: HashSet<&'a str>,
    planter: Option<&'a str>,
    defuser: Option<&'a str>,
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

fn or_lineup<'s, 'a>(alive: &'s [&'a str], lineup: &'s [&'a str]) -> &'s [&'a str] {
    if alive.is_empty() { lineup } else { alive }
}

fn credit_round<'a>(r: &RoundCtx<'a>, model: &WinModel) -> RoundCredit<'a> {
    let (start, steps) = replay(r);
    let weight = |gaining_side_attacks: bool| {
        if gaining_side_attacks == r.attackers_won { 1.0 } else { LOST_ROUND_WEIGHT }
    };
    let mut c = RoundCredit {
        wpa: HashMap::new(),
        kills: Vec::new(),
        survivors: Vec::new(),
        clutchers: HashSet::new(),
        planter: None,
        defuser: None,
    };
    let (mut alive_att, mut alive_def) = (r.start_att.clone(), r.start_def.clone());
    let mut p_last = model.p_attack(start);

    for step in steps {
        let p_before = model.p_attack(step.before);
        let p_after = match step.kind {
            StepKind::Defuse(_) => 0.0,
            _ => model.p_attack(step.after),
        };
        let swing = p_after - p_before; // attackers' perspective
        match &step.kind {
            StepKind::Kill(k) => {
                let (killer, victim) = (k.killer.puuid.as_str(), k.victim.puuid.as_str());
                let victim_att = r.side[victim];
                // A thinly sampled state can make a kill look harmful to the killer's side; never
                // credit that as negative.
                let raw_gain = (if victim_att { -swing } else { swing }).max(0.0);
                let gain = raw_gain * weight(!victim_att);
                *c.wpa.entry(victim).or_default() -= gain;
                if r.side.get(killer) != Some(&!victim_att) {
                    // Team kill, fall, own ability: the opposing survivors take the gain.
                    let opponents = if victim_att { &step.alive_def } else { &step.alive_att };
                    split(&mut c.wpa, opponents, gain);
                } else {
                    credit_kill(r, &mut c, k, raw_gain, gain, victim_att);
                }
            }
            StepKind::Plant(p) => {
                let gain = swing.max(0.0) * weight(true);
                *c.wpa.entry(p.player.puuid.as_str()).or_default() += gain;
                split(&mut c.wpa, &step.alive_def, -gain);
                c.planter = Some(&p.player.puuid);
            }
            StepKind::Defuse(d) => {
                *c.wpa.entry(d.player.puuid.as_str()).or_default() -= swing;
                split(&mut c.wpa, or_lineup(&step.alive_att, &r.start_att), swing);
                c.defuser = Some(&d.player.puuid);
            }
            StepKind::Revive => {}
        }
        p_last = p_after;
        alive_att = step.alive_att;
        alive_def = step.alive_def;
        for (mine, theirs) in [(&alive_att, &alive_def), (&alive_def, &alive_att)] {
            if mine.len() == 1 && theirs.len() >= 2 {
                c.clutchers.insert(mine[0]);
            }
        }
    }

    // Whatever the events didn't explain (detonation, time, the clock running down) goes to the
    // living; a wiped side's share goes to its whole lineup so the round still nets to zero.
    let residual = if r.attackers_won { 1.0 } else { 0.0 } - p_last;
    split(&mut c.wpa, or_lineup(&alive_att, &r.start_att), residual);
    split(&mut c.wpa, or_lineup(&alive_def, &r.start_def), -residual);
    c.survivors = alive_att.into_iter().chain(alive_def).collect();
    c
}

/// Splits one kill's gain among final blow, damage dealers and utility assisters, then refunds a
/// quickly traded teammate.
fn credit_kill<'a>(
    r: &RoundCtx<'a>,
    c: &mut RoundCredit<'a>,
    k: &'a KillDto,
    raw_gain: f64,
    gain: f64,
    victim_att: bool,
) {
    let (killer, victim) = (k.killer.puuid.as_str(), k.victim.puuid.as_str());
    let damage_to = |p: &str| r.damage.get(&(p, victim)).copied().unwrap_or(0);
    let assists: Vec<&str> = k
        .assistants
        .iter()
        .map(|a| a.puuid.as_str())
        .filter(|a| *a != killer && r.side.get(a) == Some(&!victim_att))
        .collect();
    let util_assists: Vec<&str> = assists.iter().copied().filter(|a| damage_to(a) == 0).collect();
    let dealers: Vec<(&str, f64)> = r
        .side
        .iter()
        .filter(|(_, att)| **att != victim_att)
        .map(|(p, _)| (*p, damage_to(p) as f64))
        .filter(|(_, d)| *d > 0.0)
        .collect();

    let util_share = if util_assists.is_empty() { 0.0 } else { UTIL_SHARE };
    let damage_pool = gain * (DAMAGE_SHARE + UTIL_SHARE - util_share);
    let total_damage: f64 = dealers.iter().map(|(_, d)| d).sum();
    let mut killer_credit = gain * FINAL_BLOW;
    if total_damage > 0.0 {
        for (p, d) in &dealers {
            let share = damage_pool * d / total_damage;
            if *p == killer {
                killer_credit += share;
            } else {
                *c.wpa.entry(p).or_default() += share;
            }
        }
    } else {
        killer_credit += damage_pool;
    }
    split(&mut c.wpa, &util_assists, gain * util_share);

    // Quick trade: the traded teammate earned part of this kill by drawing the fight.
    let traded = c.kills.iter().rev().find(|e| {
        e.killer() == victim && k.time_in_round_in_ms - e.t() <= REFUND_WINDOW_MS
    });
    if let Some(e) = traded {
        let refund = REFUND_SHARE * e.loss.min(killer_credit);
        killer_credit -= refund;
        *c.wpa.entry(e.dto.victim.puuid.as_str()).or_default() += refund;
    }
    *c.wpa.entry(killer).or_default() += killer_credit;
    c.kills.push(KillRecord { dto: k, raw_gain, loss: gain, assists, util_assists });
}

// ---- Per-player evaluation ------------------------------------------------------------------

fn point(p: &PointDto) -> (f32, f32) {
    (p.x, p.y)
}

fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// Everyone alive at a kill: the snapshot plus the victim at the kill location.
fn kill_positions(k: &KillDto) -> Vec<(&str, (f32, f32))> {
    let mut out: Vec<(&str, (f32, f32))> =
        k.player_locations.iter().map(|l| (l.player.puuid.as_str(), point(&l.location))).collect();
    if let Some(loc) = &k.location {
        out.push((&k.victim.puuid, point(loc)));
    }
    out
}

/// Whether `me` is far from every alive teammate. `None` if `me` isn't in the snapshot or has no
/// teammate alive.
fn isolated(me: &str, positions: &[(&str, (f32, f32))], side: &HashMap<&str, bool>) -> Option<bool> {
    let my_side = side.get(me)?;
    let my_pos = positions.iter().find(|(p, _)| *p == me)?.1;
    let nearest = positions
        .iter()
        .filter(|(p, _)| *p != me && side.get(p) == Some(my_side))
        .map(|(_, pos)| dist(my_pos, *pos))
        .min_by(f32::total_cmp)?;
    Some(nearest > LURK_UNITS)
}

#[derive(Default)]
struct Tally {
    impact: Impact,
    wpa: f64,
    kast_rounds: u32,
    lurk_swing_sum: f64,
    casts: u32,
    cast_rounds: u32,
    agents: HashMap<String, u32>,
    entry_spots: HashMap<String, u32>,
}

/// Impact for `puuid` across `matches` (newest first). `assets` enables site-entry detection.
/// `None` when the player has no playable rounds in the sample.
pub fn evaluate(matches: &[MatchDto], puuid: &str, model: &WinModel, assets: Option<&Assets>) -> Option<Impact> {
    let mut t = Tally::default();
    let mut strip: Vec<Vec<RoundSwing>> = Vec::new();

    for (mi, m) in matches.iter().enumerate() {
        let played = rounds_played(m);
        let Some(&my_rounds) = played.get(puuid) else { continue };
        t.impact.matches += 1;
        if let Some(p) = m.players.iter().find(|p| p.puuid == puuid) {
            t.casts += p.ability_casts.total();
            t.cast_rounds += my_rounds;
            if let Some(agent) = &p.agent {
                *t.agents.entry(agent.id.to_ascii_lowercase()).or_default() += 1;
            }
        }
        let map = m.metadata.map.as_ref().map(|m| m.id.to_ascii_lowercase());
        let mut match_strip = Vec::new();
        for r in rounds(m) {
            if !r.start_att.contains(&puuid) && !r.start_def.contains(&puuid) {
                continue; // absent or AFK this round
            }
            let swing = evaluate_round(&r, puuid, model, assets, map.as_deref(), &mut t);
            if mi < STRIP_MATCHES {
                match_strip.push(swing);
            }
        }
        if let Some(first) = match_strip.first_mut() {
            first.match_start = true;
        }
        strip.push(match_strip);
    }
    if t.impact.rounds == 0 {
        return None;
    }

    let mut out = t.impact;
    let shrunk = t.wpa / (out.rounds as f64 + SHRINK_ROUNDS);
    out.wpa_per_round = shrunk as f32;
    out.rating = model.rating(shrunk) as f32;
    out.kast = t.kast_rounds as f32 / out.rounds as f32;
    out.casts_per_round = t.casts as f32 / t.cast_rounds.max(1) as f32;
    out.main_agent = t.agents.into_iter().max_by_key(|(_, n)| *n).map(|(a, _)| a);
    out.agent_casts_per_round = out.main_agent.as_deref().and_then(|a| model.agent_casts_per_round(a));
    out.entry_spot = t.entry_spots.into_iter().max_by_key(|(_, n)| *n).map(|(s, _)| s);
    out.lurk_swing = if out.lurk_rounds == 0 { 0.0 } else { (t.lurk_swing_sum / out.lurk_rounds as f64) as f32 };
    // Matches arrive newest first; the strip reads oldest to newest.
    out.recent_rounds = strip.into_iter().take(STRIP_MATCHES).rev().flatten().collect();
    Some(out)
}

fn evaluate_round(
    r: &RoundCtx<'_>,
    me: &str,
    model: &WinModel,
    assets: Option<&Assets>,
    map: Option<&str>,
    t: &mut Tally,
) -> RoundSwing {
    let c = credit_round(r, model);
    let attacker = r.side[me];
    let my_swing = c.wpa.get(me).copied().unwrap_or(0.0);
    let plant_t = r.plant.map(|p| p.round_time_in_ms);
    let out = &mut t.impact;
    out.rounds += 1;
    t.wpa += my_swing;

    let mut kast = c.survivors.contains(&me);
    let mut lurked = false;
    let mut entry_decided = false;
    let mut kills = 0u8;
    let mut died = false;

    if let Some(first) = c.kills.first() {
        out.opening_wins += (first.killer() == me) as u32;
        out.opening_losses += (first.victim() == me) as u32;
    }

    for (i, k) in c.kills.iter().enumerate() {
        let time = k.t();
        let pre_plant = plant_t.is_none_or(|p| time < p);
        let positions = kill_positions(k.dto);
        let alone = attacker && pre_plant && isolated(me, &positions, &r.side) == Some(true);
        // A lurk is fighting alone, not merely being far away while others fight.
        lurked |= alone && (k.killer() == me || k.victim() == me);
        let traded_later = c.kills[i + 1..].iter().any(|e| e.victim() == k.killer() && e.t() - time <= TRADE_WINDOW_MS);

        if k.killer() == me {
            out.kills += 1;
            kills = kills.saturating_add(1);
            kast = true;
            if k.raw_gain < LOW_IMPACT {
                out.low_impact_kills += 1;
                out.lurk_late_kills += alone as u32;
            }
            out.impact_frags += (i > 0 && k.raw_gain >= IMPACT_FRAG) as u32;
            let ability = k.dto.weapon.as_ref().and_then(|w| w.kind.as_deref()) == Some("Ability");
            out.ability_kills += ability as u32;
            // Fastest teammate death this kill avenged.
            let avenged = c.kills[..i]
                .iter()
                .filter(|e| e.killer() == k.victim() && time - e.t() <= TRADE_WINDOW_MS)
                .map(|e| time - e.t())
                .min();
            match avenged {
                Some(dt) if dt <= FAST_TRADE_MS => out.trades_fast += 1,
                Some(_) => out.trades_late += 1,
                None => {}
            }
        }
        if k.assists.contains(&me) {
            kast = true;
        }
        out.util_assists += k.util_assists.contains(&me) as u32;

        if k.victim() == me {
            out.deaths += 1;
            died = true;
            if traded_later {
                out.deaths_traded += 1;
                kast = true;
            } else if alone {
                out.lurk_deaths_untraded += 1;
            }
        } else if r.side.get(k.victim()) == Some(&attacker) && !traded_later {
            // A teammate died close by while I was alive, and nobody punished the killer.
            let mine = positions.iter().find(|(p, _)| *p == me).map(|x| x.1);
            let theirs = k.dto.location.as_ref().map(point);
            if let (Some(a), Some(b)) = (mine, theirs)
                && dist(a, b) <= NEAR_UNITS
            {
                out.missed_refrags += 1;
            }
        }

        // The attackers' first fight inside a site area before the plant.
        if !entry_decided
            && pre_plant
            && let (Some(assets), Some(map), Some(loc)) = (assets, map, k.dto.location.as_ref())
            && let Some(callout) = assets.callout(map, loc.x, loc.y)
            && callout.is_site()
        {
            entry_decided = true;
            if attacker && k.killer() == me {
                out.entry_wins += 1;
                *t.entry_spots.entry(callout.label()).or_default() += 1;
            } else if attacker && k.victim() == me {
                out.entry_losses += 1;
            }
        }
    }

    // Planting with no teammate within ~30 m is a solo (ninja/lurk) plant.
    if let Some(p) = r.plant.filter(|p| p.player.puuid == me) {
        let positions: Vec<(&str, (f32, f32))> =
            p.player_locations.iter().map(|l| (l.player.puuid.as_str(), point(&l.location))).collect();
        lurked |= isolated(me, &positions, &r.side) == Some(true);
    }
    if lurked {
        out.lurk_rounds += 1;
        t.lurk_swing_sum += my_swing;
    }
    if c.clutchers.contains(me) {
        out.clutches += 1;
        out.clutches_won += (attacker == r.attackers_won) as u32;
    }
    t.kast_rounds += kast as u32;

    RoundSwing {
        swing: my_swing as f32,
        won: attacker == r.attackers_won,
        kills,
        died,
        planted: c.planter == Some(me),
        defused: c.defuser == Some(me),
        match_start: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct K<'a> {
        round: u32,
        t: u32,
        killer: &'a str,
        victim: &'a str,
        assist: &'a str,
        /// Victim position x (y = 0); killer and others are placed by the test.
        vx: i32,
    }

    fn team(p: &str) -> &'static str {
        if p.starts_with('a') { "Red" } else { "Blue" }
    }

    /// (winning team, winning role, plant time, (defuse time, defuser)).
    type RoundSpec<'a> = (&'a str, &'a str, Option<u32>, Option<(u32, &'a str)>);

    /// Red attacks both rounds. `damage` lists (round, dealer, target, amount).
    fn build(kills: &[K], rounds: &[RoundSpec], damage: &[(u32, &str, &str, u32)]) -> MatchDto {
        let players: Vec<String> = (1..=5).map(|i| format!("a{i}")).chain((1..=5).map(|i| format!("d{i}"))).collect();
        let rounds_json: Vec<String> = rounds
            .iter()
            .enumerate()
            .map(|(ri, (winner, role, plant, defuse))| {
                let stats: Vec<String> = players
                    .iter()
                    .map(|p| {
                        let dmg: Vec<String> = damage
                            .iter()
                            .filter(|(r, d, _, _)| *r as usize == ri && d == p)
                            .map(|(_, _, t, n)| format!(r#"{{"player":{{"puuid":"{t}","team":"{}"}},"damage":{n}}}"#, team(t)))
                            .collect();
                        format!(
                            r#"{{"player":{{"puuid":"{p}","team":"{}"}},"economy":{{"loadout_value":3900}},"damage_events":[{}]}}"#,
                            team(p),
                            dmg.join(",")
                        )
                    })
                    .collect();
                let spike = |t: u32, who: &str| {
                    format!(r#"{{"round_time_in_ms":{t},"player":{{"puuid":"{who}","team":"{}"}}}}"#, team(who))
                };
                let plant = plant.map_or("null".to_owned(), |t| spike(t, "a2"));
                let defuse = defuse.map_or("null".to_owned(), |(t, who)| spike(t, who));
                format!(
                    r#"{{"winning_team":"{winner}","winning_team_role":"{role}","plant":{plant},"defuse":{defuse},"stats":[{}]}}"#,
                    stats.join(",")
                )
            })
            .collect();
        let kills_json: Vec<String> = kills
            .iter()
            .map(|k| {
                let assists = if k.assist.is_empty() {
                    String::new()
                } else {
                    format!(r#"{{"puuid":"{}","team":"{}"}}"#, k.assist, team(k.assist))
                };
                // Killer stands 500 units from the victim; everyone else at x = 0.
                let locs: Vec<String> = players
                    .iter()
                    .filter(|p| *p != k.victim)
                    .map(|p| {
                        let x = if *p == k.killer { k.vx + 500 } else { 0 };
                        format!(r#"{{"player":{{"puuid":"{p}","team":"{}"}},"location":{{"x":{x},"y":0}}}}"#, team(p))
                    })
                    .collect();
                format!(
                    r#"{{"round":{},"time_in_round_in_ms":{},"killer":{{"puuid":"{}","team":"{}"}},
                       "victim":{{"puuid":"{}","team":"{}"}},"assistants":[{assists}],
                       "location":{{"x":{},"y":0}},"weapon":{{"type":"Weapon"}},"player_locations":[{}]}}"#,
                    k.round,
                    k.t,
                    k.killer,
                    team(k.killer),
                    k.victim,
                    team(k.victim),
                    k.vx,
                    locs.join(",")
                )
            })
            .collect();
        let players_json: Vec<String> = players
            .iter()
            .map(|p| format!(r#"{{"puuid":"{p}","agent":{{"id":"agent-{p}"}},"ability_casts":{{"grenade":2,"ability1":1}}}}"#))
            .collect();
        let json = format!(
            r#"{{"metadata":{{"match_id":"m1","map":{{"id":"map"}}}},"players":[{}],"rounds":[{}],"kills":[{}]}}"#,
            players_json.join(","),
            rounds_json.join(","),
            kills_json.join(",")
        );
        serde_json::from_str(&json).unwrap()
    }

    fn k(round: u32, t: u32, killer: &'static str, victim: &'static str) -> K<'static> {
        K { round, t, killer, victim, assist: "", vx: 0 }
    }

    /// Round 0 (Red attacks and wins): a1 opens, d2 kills a1 (d3 flash-assists), a2 trades within
    /// 1 s, a3–a5 die, a2 plants and wins a 1v3. Round 1: d5 defuses.
    fn sample() -> MatchDto {
        let kills = [
            k(0, 10_000, "a1", "d1"),
            K { assist: "d3", ..k(0, 12_000, "d2", "a1") },
            k(0, 13_000, "a2", "d2"),
            k(0, 20_000, "d3", "a3"),
            k(0, 21_000, "d3", "a4"),
            k(0, 22_000, "d4", "a5"),
            k(0, 40_000, "a2", "d3"),
            k(0, 41_000, "a2", "d4"),
            k(1, 30_000, "d1", "a1"),
        ];
        let rounds = [("Red", "Attacker", Some(30_000), None), ("Blue", "Defender", Some(25_000), Some((60_000, "d5")))];
        // d4 chipped a1 before d2's kill; d3 only flashed.
        let damage = [(0, "d2", "a1", 100), (0, "d4", "a1", 50), (0, "a2", "d2", 150)];
        build(&kills, &rounds, &damage)
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
    fn credits_openings_trades_clutches_and_utility() {
        let m = sample();
        let model = WinModel::default();
        let a2 = evaluate(std::slice::from_ref(&m), "a2", &model, None).unwrap();
        assert_eq!((a2.rounds, a2.kills, a2.trades_fast, a2.trades_late), (2, 3, 1, 0));
        assert_eq!((a2.clutches, a2.clutches_won), (1, 1), "a2 won a 1v3");
        assert!(a2.rating > 1.0);

        let a1 = evaluate(std::slice::from_ref(&m), "a1", &model, None).unwrap();
        assert_eq!((a1.opening_wins, a1.opening_losses, a1.deaths_traded), (1, 1, 1));

        let d3 = evaluate(std::slice::from_ref(&m), "d3", &model, None).unwrap();
        assert_eq!(d3.util_assists, 1, "an assist without damage is a utility assist");

        let d5 = evaluate(std::slice::from_ref(&m), "d5", &model, None).unwrap();
        assert!(d5.wpa_per_round > 0.0, "the defuse is credited even without kills");
        assert_eq!(d5.recent_rounds.len(), 2);
        assert!(d5.recent_rounds[0].match_start && d5.recent_rounds[1].defused);
    }

    #[test]
    fn kill_credit_follows_damage_utility_and_trades() {
        let m = sample();
        let model = WinModel::default();
        let r = &rounds(&m)[0];
        let c = credit_round(r, &model);
        let d2_kill = c.kills.iter().find(|k| k.victim() == "a1").unwrap();
        assert_eq!(d2_kill.util_assists, vec!["d3"]);

        // d4 never touched the kill except by damage and still earns part of it.
        let solo = build(&[k(0, 12_000, "d2", "a1")], &[("Blue", "Defender", None, None)], &[(0, "d2", "a1", 100)]);
        let shared = build(
            &[k(0, 12_000, "d2", "a1")],
            &[("Blue", "Defender", None, None)],
            &[(0, "d2", "a1", 100), (0, "d4", "a1", 50)],
        );
        let credit = |m: &MatchDto, p: &str| credit_round(&rounds(m)[0], &model).wpa.get(p).copied().unwrap_or(0.0);
        assert!(credit(&shared, "d4") > credit(&solo, "d4"), "damage share is credited");
        assert!(credit(&shared, "d2") < credit(&solo, "d2"), "the final blow is shared");

        // a1's death was traded within 1 s: refunded relative to the same death untraded.
        let untraded = build(
            &[k(0, 10_000, "a1", "d1"), k(0, 12_000, "d2", "a1")],
            &[("Red", "Attacker", None, None)],
            &[],
        );
        let traded = build(
            &[k(0, 10_000, "a1", "d1"), k(0, 12_000, "d2", "a1"), k(0, 13_000, "a2", "d2")],
            &[("Red", "Attacker", None, None)],
            &[],
        );
        let without_trade_kill = credit(&untraded, "a1");
        assert!(credit(&traded, "a1") > without_trade_kill, "traded death gets a refund");
    }

    #[test]
    fn swings_for_the_losing_side_count_half() {
        let model = WinModel::default();
        // The victim is dead at round end, so their credit is exactly the weighted swing.
        let opening_loss = |winner: &str, role: &str| {
            let m = build(&[k(0, 10_000, "a1", "d1")], &[(winner, role, None, None)], &[(0, "a1", "d1", 150)]);
            -credit_round(&rounds(&m)[0], &model).wpa["d1"]
        };
        let attackers_won = opening_loss("Red", "Attacker");
        let attackers_lost = opening_loss("Blue", "Defender");
        assert!(
            attackers_lost > 0.0 && (attackers_won - 2.0 * attackers_lost).abs() < 1e-9,
            "won {attackers_won}, lost {attackers_lost}"
        );
    }

    #[test]
    fn late_post_plant_kills_are_low_impact_and_isolation_counts_as_lurk() {
        let model = WinModel::default();
        // Spike planted at 30 s; d1 kills a1 at 70 s (5 s on the spike): the round is decided.
        let late = build(
            &[K { vx: 0, ..k(0, 70_000, "d1", "a1") }],
            &[("Red", "Attacker", Some(30_000), None)],
            &[],
        );
        let d1 = evaluate(std::slice::from_ref(&late), "d1", &model, None).unwrap();
        assert_eq!(d1.low_impact_kills, 1);

        // a1 kills d1 5,000 units from every teammate before any plant: a lurk kill.
        let lurk = build(&[K { vx: 5_000, ..k(0, 20_000, "a1", "d1") }], &[("Red", "Attacker", None, None)], &[]);
        let a1 = evaluate(std::slice::from_ref(&lurk), "a1", &model, None).unwrap();
        assert_eq!(a1.lurk_rounds, 1);
    }

    #[test]
    fn missed_refrag_needs_proximity_and_no_trade() {
        let model = WinModel::default();
        // a1 dies at x = 0 next to the others; nobody trades d1 within 5 s.
        let m = build(&[k(0, 10_000, "d1", "a1"), k(0, 30_000, "a2", "d1")], &[("Red", "Attacker", None, None)], &[]);
        let a3 = evaluate(std::slice::from_ref(&m), "a3", &model, None).unwrap();
        assert_eq!(a3.missed_refrags, 1);
        // Same death traded after 2 s: nobody missed it.
        let m = build(&[k(0, 10_000, "d1", "a1"), k(0, 12_000, "a2", "d1")], &[("Red", "Attacker", None, None)], &[]);
        let a3 = evaluate(std::slice::from_ref(&m), "a3", &model, None).unwrap();
        assert_eq!(a3.missed_refrags, 0);
    }

    #[test]
    fn model_learns_each_match_once_and_tracks_agent_util() {
        let m = sample();
        let mut model = WinModel::default();
        let start = State { a: 5, d: 5, planted: false, econ: 0, time: 0 };
        assert!((model.p_attack(start) - 0.5).abs() < 1e-9);
        assert!(model.ingest(std::slice::from_ref(&m)));
        assert!(!model.ingest(std::slice::from_ref(&m)), "same match id is ignored");
        assert_eq!(model.cells[&start], [2, 1]);
        assert_eq!(model.p_attack(State { d: 0, ..start }), 1.0);
        assert_eq!(model.agent_casts_per_round("agent-a1"), Some(1.5), "3 casts over 2 rounds");
    }

    #[test]
    fn time_buckets_follow_round_clock_and_spike_timer() {
        assert_eq!(time_bucket(10_000, None), 0);
        assert_eq!(time_bucket(80_000, None), 2);
        assert_eq!(time_bucket(95_000, None), 3);
        assert_eq!(time_bucket(40_000, Some(30_000)), 0);
        assert_eq!(time_bucket(70_000, Some(30_000)), 3, "5 s on the spike: no time to defuse");
    }
}
