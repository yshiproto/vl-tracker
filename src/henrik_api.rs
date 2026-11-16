//! HenrikDev API client (`https://api.henrikdev.xyz`).
//!
//! HenrikDev counts its own background Riot calls against the key's budget, so request cost is
//! not knowable up front. The gate trusts the server: it follows `X-RateLimit-Remaining` /
//! `X-RateLimit-Reset`, pauses at zero, and honours `Retry-After` on 429.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::{Semaphore, SemaphorePermit};

use crate::impact::MatchDto;
use crate::model::{ActRank, Error, Rank, RankProfile, Result, Stats};

const BASE: &str = "https://api.henrikdev.xyz/valorant/";
/// Matches used for the per-player stat summary.
const STATS_MATCHES: u32 = 20;
/// Full matches used for the impact rating.
const IMPACT_MATCHES: u32 = 5;
/// Full-match requests can make HenrikDev fetch several matches from Riot first.
const SLOW_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_IN_FLIGHT: usize = 3;
const MAX_ATTEMPTS: u32 = 3;

pub struct Account {
    pub puuid: String,
    pub riot_id: String,
    pub region: String,
}

#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
}

#[derive(Deserialize)]
struct AccountDto {
    puuid: String,
    name: String,
    tag: String,
    region: String,
}

#[derive(Deserialize)]
struct MmrDto {
    current: CurrentMmrDto,
    #[serde(default)]
    seasonal: Vec<SeasonalDto>,
}

#[derive(Deserialize)]
struct CurrentMmrDto {
    tier: TierDto,
    #[serde(default)]
    rr: u32,
}

#[derive(Deserialize)]
struct TierDto {
    id: u32,
}

#[derive(Deserialize)]
struct SeasonalDto {
    season: SeasonRefDto,
    end_tier: TierDto,
    #[serde(default)]
    games: u32,
    #[serde(default)]
    wins: u32,
    /// One entry per win, at the tier it was won.
    #[serde(default)]
    act_wins: Vec<TierDto>,
}

#[derive(Deserialize)]
struct SeasonRefDto {
    id: String,
}

#[derive(Deserialize)]
struct StoredMatchDto {
    stats: StoredStatsDto,
    #[serde(default)]
    teams: RoundsDto,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StoredStatsDto {
    team: String,
    score: u32,
    kills: u32,
    deaths: u32,
    shots: ShotsDto,
    damage: DamageDto,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ShotsDto {
    head: u32,
    body: u32,
    leg: u32,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DamageDto {
    made: u32,
}

/// Rounds won per team.
#[derive(Deserialize, Default)]
#[serde(default)]
struct RoundsDto {
    red: u32,
    blue: u32,
}

struct Budget {
    remaining: Option<u32>,
    reset_at: Instant,
}

pub struct Henrik {
    http: reqwest::Client,
    key: Mutex<String>,
    in_flight: Semaphore,
    budget: Mutex<Budget>,
}

impl Henrik {
    pub fn new(http: reqwest::Client, key: String) -> Self {
        Self {
            http,
            key: Mutex::new(key),
            in_flight: Semaphore::new(MAX_IN_FLIGHT),
            budget: Mutex::new(Budget { remaining: None, reset_at: Instant::now() }),
        }
    }

    pub fn set_key(&self, key: String) {
        *self.key.lock().unwrap_or_else(|e| e.into_inner()) = key;
    }

    pub async fn account(&self, name: &str, tag: &str) -> Result<Account> {
        let dto: AccountDto = self.get(&["v2", "account", name, tag], &[], None).await?;
        Ok(Account { puuid: dto.puuid, riot_id: format!("{}#{}", dto.name, dto.tag), region: dto.region })
    }

    pub async fn account_by_puuid(&self, puuid: &str) -> Result<Account> {
        let dto: AccountDto = self.get(&["v2", "by-puuid", "account", puuid], &[], None).await?;
        Ok(Account { puuid: dto.puuid, riot_id: format!("{}#{}", dto.name, dto.tag), region: dto.region })
    }

    pub async fn rank(&self, region: &str, puuid: &str) -> Result<RankProfile> {
        let dto: MmrDto = self.get(&["v3", "by-puuid", "mmr", region, "pc", puuid], &[], None).await?;
        let acts = dto
            .seasonal
            .into_iter()
            .filter(|s| s.games > 0)
            .map(|s| ActRank {
                act: s.season.id.to_ascii_lowercase(),
                peak_tier: s.act_wins.iter().map(|t| t.id).max().unwrap_or(0).max(s.end_tier.id),
                tier: s.end_tier.id,
                wins: s.wins,
                games: s.games,
            })
            .collect();
        Ok(RankProfile { current: Rank { tier: dto.current.tier.id, rr: dto.current.rr }, acts })
    }

    /// Summary over the last `STATS_MATCHES` competitive games.
    pub async fn stats(&self, region: &str, puuid: &str) -> Result<Stats> {
        let size = STATS_MATCHES.to_string();
        let matches: Vec<StoredMatchDto> = self
            .get(
                &["v1", "by-puuid", "stored-matches", region, puuid],
                &[("mode", "competitive"), ("size", &size)],
                None,
            )
            .await?;
        aggregate(&matches).ok_or(Error::NotFound)
    }

    /// Full timelines of the last `IMPACT_MATCHES` competitive games (costs 1 plus 1 per match
    /// HenrikDev has not stored yet).
    pub async fn recent_matches(&self, region: &str, puuid: &str) -> Result<Vec<MatchDto>> {
        let size = IMPACT_MATCHES.to_string();
        self.get(
            &["v4", "by-puuid", "matches", region, "pc", puuid],
            &[("mode", "competitive"), ("size", &size)],
            Some(SLOW_TIMEOUT),
        )
        .await
    }

    async fn get<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        query: &[(&str, &str)],
        timeout: Option<Duration>,
    ) -> Result<T> {
        let key = self.key.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if key.is_empty() {
            return Err(Error::NoApiKey);
        }
        let mut url = Url::parse(BASE).expect("static base URL");
        url.path_segments_mut().expect("base URL has a path").pop_if_empty().extend(segments);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }

        for _ in 0..MAX_ATTEMPTS {
            let _permit = self.acquire().await;
            let mut req = self.http.get(url.clone()).header("Authorization", &key);
            if let Some(t) = timeout {
                req = req.timeout(t);
            }
            let resp = req.send().await?;
            self.observe(&resp);
            match resp.status() {
                s if s.is_success() => return Ok(resp.json::<Envelope<T>>().await?.data),
                // `observe` has already parked the budget until the reset time.
                StatusCode::TOO_MANY_REQUESTS => continue,
                StatusCode::NOT_FOUND => return Err(Error::NotFound),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return Err(Error::InvalidApiKey),
                s => return Err(Error::Status(s)),
            }
        }
        Err(Error::RateLimited)
    }

    /// Bounds concurrency and waits out an exhausted window.
    async fn acquire(&self) -> SemaphorePermit<'_> {
        let permit = self.in_flight.acquire().await.expect("semaphore never closed");
        loop {
            let wait = {
                let mut b = self.budget.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                match b.remaining {
                    Some(0) if now < b.reset_at => Some(b.reset_at - now),
                    Some(0) => {
                        b.remaining = None;
                        None
                    }
                    // Optimistic local decrement; the response headers correct it.
                    Some(n) => {
                        b.remaining = Some(n - 1);
                        None
                    }
                    None => None,
                }
            };
            match wait {
                Some(d) => tokio::time::sleep(d).await,
                None => return permit,
            }
        }
    }

    fn observe(&self, resp: &reqwest::Response) {
        let header = |name: &str| -> Option<u64> { resp.headers().get(name)?.to_str().ok()?.trim().parse().ok() };
        let mut b = self.budget.lock().unwrap_or_else(|e| e.into_inner());
        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            let secs = header("retry-after").or_else(|| header("x-ratelimit-reset")).unwrap_or(60);
            b.remaining = Some(0);
            b.reset_at = Instant::now() + Duration::from_secs(secs.max(1));
            return;
        }
        if let Some(remaining) = header("x-ratelimit-remaining") {
            b.remaining = Some(remaining as u32);
        }
        if let Some(reset) = header("x-ratelimit-reset") {
            b.reset_at = Instant::now() + Duration::from_secs(reset);
        }
    }
}

fn aggregate(matches: &[StoredMatchDto]) -> Option<Stats> {
    if matches.is_empty() {
        return None;
    }
    let (mut kills, mut deaths, mut head, mut shots) = (0u32, 0u32, 0u32, 0u32);
    let (mut score, mut damage, mut rounds, mut wins, mut losses) = (0u32, 0u32, 0u32, 0u32, 0u32);
    for m in matches {
        let s = &m.stats;
        kills += s.kills;
        deaths += s.deaths;
        head += s.shots.head;
        shots += s.shots.head + s.shots.body + s.shots.leg;
        score += s.score;
        damage += s.damage.made;
        rounds += m.teams.red + m.teams.blue;
        let (mine, theirs) = if s.team.eq_ignore_ascii_case("red") {
            (m.teams.red, m.teams.blue)
        } else {
            (m.teams.blue, m.teams.red)
        };
        match mine.cmp(&theirs) {
            std::cmp::Ordering::Greater => wins += 1,
            std::cmp::Ordering::Less => losses += 1,
            std::cmp::Ordering::Equal => {}
        }
    }
    let per_round = |total: u32| if rounds == 0 { 0.0 } else { total as f32 / rounds as f32 };
    Some(Stats {
        kd: kills as f32 / deaths.max(1) as f32,
        hs_pct: if shots == 0 { 0.0 } else { head as f32 * 100.0 / shots as f32 },
        matches: matches.len() as u32,
        wins,
        losses,
        acs: per_round(score),
        adr: per_round(damage),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_win_rate_and_per_round_stats_from_stored_matches() {
        let json = r#"[
          {"stats":{"team":"Red","score":4000,"kills":20,"deaths":10,"shots":{"head":10,"body":30,"leg":0},
                    "damage":{"made":3000}},"teams":{"red":13,"blue":7}},
          {"stats":{"team":"Blue","score":2000,"kills":10,"deaths":20,"shots":{"head":0,"body":10,"leg":0},
                    "damage":{"made":1000}},"teams":{"red":13,"blue":7}},
          {"stats":{"team":"Blue","score":2000,"kills":0,"deaths":0},"teams":{"red":12,"blue":12}}
        ]"#;
        let matches: Vec<StoredMatchDto> = serde_json::from_str(json).unwrap();
        let s = aggregate(&matches).unwrap();
        assert_eq!((s.wins, s.losses, s.matches), (1, 1, 3)); // draw counts as neither
        assert_eq!(s.kd, 1.0);
        assert_eq!(s.hs_pct, 20.0);
        assert_eq!(s.acs, 8000.0 / 64.0);
        assert_eq!(s.adr, 4000.0 / 64.0);
        assert!(aggregate(&[]).is_none());
    }
}
