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

use crate::model::{Error, Rank, Result, Stats};

const BASE: &str = "https://api.henrikdev.xyz/valorant/";
/// Matches used for K/D and HS%.
const STATS_MATCHES: u32 = 20;
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
struct StoredMatchDto {
    stats: StoredStatsDto,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StoredStatsDto {
    kills: u32,
    deaths: u32,
    shots: ShotsDto,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ShotsDto {
    head: u32,
    body: u32,
    leg: u32,
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
        let dto: AccountDto = self.get(&["v2", "account", name, tag], &[]).await?;
        Ok(Account { puuid: dto.puuid, riot_id: format!("{}#{}", dto.name, dto.tag), region: dto.region })
    }

    pub async fn account_by_puuid(&self, puuid: &str) -> Result<Account> {
        let dto: AccountDto = self.get(&["v2", "by-puuid", "account", puuid], &[]).await?;
        Ok(Account { puuid: dto.puuid, riot_id: format!("{}#{}", dto.name, dto.tag), region: dto.region })
    }

    pub async fn rank(&self, region: &str, puuid: &str) -> Result<Rank> {
        let dto: MmrDto = self.get(&["v3", "by-puuid", "mmr", region, "pc", puuid], &[]).await?;
        Ok(Rank { tier: dto.current.tier.id, rr: dto.current.rr })
    }

    /// K/D and headshot % over the last `STATS_MATCHES` competitive games.
    pub async fn stats(&self, region: &str, puuid: &str) -> Result<Stats> {
        let size = STATS_MATCHES.to_string();
        let matches: Vec<StoredMatchDto> = self
            .get(
                &["v1", "by-puuid", "stored-matches", region, puuid],
                &[("mode", "competitive"), ("size", &size)],
            )
            .await?;
        if matches.is_empty() {
            return Err(Error::NotFound);
        }
        let (mut kills, mut deaths, mut head, mut shots) = (0u32, 0u32, 0u32, 0u32);
        for m in &matches {
            let s = &m.stats;
            kills += s.kills;
            deaths += s.deaths;
            head += s.shots.head;
            shots += s.shots.head + s.shots.body + s.shots.leg;
        }
        Ok(Stats {
            kd: kills as f32 / deaths.max(1) as f32,
            hs_pct: if shots == 0 { 0.0 } else { head as f32 * 100.0 / shots as f32 },
            matches: matches.len() as u32,
        })
    }

    async fn get<T: DeserializeOwned>(&self, segments: &[&str], query: &[(&str, &str)]) -> Result<T> {
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
            let resp = self.http.get(url.clone()).header("Authorization", &key).send().await?;
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
