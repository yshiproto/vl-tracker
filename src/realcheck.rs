//! Manual validation harness (`cargo test realcheck -- --ignored`): real cache round-trip,
//! leave-one-match-out calibration and split-half stability. Needs HENRIK_KEY + RIOT_ID.
use std::sync::Arc;
use std::time::Instant;

use crate::henrik_api::{Henrik, IMPACT_PAGE};
use crate::impact::{self, MatchDto, WinModel};
use crate::match_cache::MatchCache;
use crate::val_assets::Assets;

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn real_matches() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let key = std::env::var("HENRIK_KEY").expect("HENRIK_KEY");
    let riot_id = std::env::var("RIOT_ID").expect("RIOT_ID");
    let (name, tag) = riot_id.rsplit_once('#').unwrap();
    let http = reqwest::Client::builder().build().unwrap();
    let assets = Assets::fetch(&http).await.expect("assets");
    let henrik = Henrik::new(http, key);
    let account = henrik.account(name, tag).await.expect("account");

    // Cache round-trip: fetch, store, reload, compare.
    let mut cache = MatchCache::load();
    let cached_ids: Vec<String> = cache.history(&account.puuid).into_iter().map(|(id, _)| id).collect();
    println!("cache has {} matches for this player", cached_ids.len());
    let mut matches: Vec<Arc<MatchDto>> = cached_ids.iter().filter_map(|id| cache.get(id)).take(20).collect();
    let mut start = matches.len() as u32;
    while matches.len() < 20 {
        let t = Instant::now();
        let page = henrik.recent_matches(&account.region, &account.puuid, start).await.expect("page");
        let n = page.len() as u32;
        println!("page start={start}: {n} matches in {:?}; quota {:?}", t.elapsed(), henrik.quota());
        matches.extend(cache.insert(page));
        start += n;
        if n < IMPACT_PAGE {
            break;
        }
    }
    let reloaded = MatchCache::load();
    let ids: Vec<String> = reloaded.history(&account.puuid).into_iter().map(|(id, _)| id).collect();
    println!("after insert, cache lists {} matches (first {:?})", ids.len(), ids.first().map(|s| &s[..8]));
    assert!(ids.len() >= matches.len());

    let mut model = WinModel::default();
    model.ingest(&matches);
    println!("{}", impact::debug_calibration(&matches, &model));

    let me = impact::evaluate(&matches, &account.puuid, &model, Some(&assets)).expect("impact");
    println!(
        "\nTARGET rating {:.2} wpa {:+.4} reliability {:.2} consistency_sd {:.3} att/def {:+.3}/{:+.3} opp_tier {:.1} gap {:+.1} rounds {} matches {}",
        me.rating, me.wpa_per_round, me.reliability, me.consistency_sd, me.attack_wpa, me.defence_wpa, me.opponent_tier, me.tier_gap, me.rounds, me.matches
    );

    // Stability: first 10 vs next 10 matches for every player with rounds in both halves.
    let (first, second) = matches.split_at(matches.len() / 2);
    let mut pairs = Vec::new();
    for p in impact::debug_players(&matches) {
        let a = impact::evaluate(first, &p, &model, Some(&assets));
        let b = impact::evaluate(second, &p, &model, Some(&assets));
        if let (Some(a), Some(b)) = (a, b)
            && a.rounds >= 15
            && b.rounds >= 15
        {
            pairs.push((a.rating, b.rating, a.kills as f32 / a.deaths.max(1) as f32, b.kills as f32 / b.deaths.max(1) as f32));
        }
    }
    let r_rating = corr(&pairs.iter().map(|p| p.0).collect::<Vec<_>>(), &pairs.iter().map(|p| p.1).collect::<Vec<_>>());
    let r_kd = corr(&pairs.iter().map(|p| p.2).collect::<Vec<_>>(), &pairs.iter().map(|p| p.3).collect::<Vec<_>>());
    println!("split-half ({} players with 15+ rounds in both halves): rating r={r_rating:.2}, K/D r={r_kd:.2}", pairs.len());

    let mut rows: Vec<_> = impact::debug_players(&matches)
        .into_iter()
        .filter_map(|p| impact::evaluate(&matches, &p, &model, Some(&assets)).map(|i| (p, i)))
        .collect();
    rows.sort_by(|a, b| b.1.rating.total_cmp(&a.1.rating));
    for (p, i) in rows.iter().take(8).chain(rows.iter().rev().take(5)) {
        println!(
            "{}… rating {:.2} rel {:.2} sd {:.3} rounds {:>3} K/D {}/{} opp_tier {:.1} gap {:+.1}",
            &p[..8], i.rating, i.reliability, i.consistency_sd, i.rounds, i.kills, i.deaths, i.opponent_tier, i.tier_gap
        );
    }
}

fn corr(x: &[f32], y: &[f32]) -> f32 {
    let n = x.len() as f32;
    let (mx, my) = (x.iter().sum::<f32>() / n, y.iter().sum::<f32>() / n);
    let cov: f32 = x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my)).sum();
    let sx = x.iter().map(|a| (a - mx).powi(2)).sum::<f32>().sqrt();
    let sy = y.iter().map(|b| (b - my).powi(2)).sum::<f32>().sqrt();
    cov / (sx * sy)
}

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn rating_spread() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut cache = MatchCache::load();
    let puuid = "e7bd5cb0-0bb2-551f-b3ad-9ab50ddd0950";
    let ids: Vec<String> = cache.history(puuid).into_iter().map(|(id, _)| id).collect();
    let matches: Vec<Arc<MatchDto>> = ids.iter().filter_map(|id| cache.get(id)).collect();
    let mut model = WinModel::default();
    model.ingest(&matches);
    let mut wpas: Vec<f32> = Vec::new();
    for p in impact::debug_players(&matches) {
        if let Some(i) = impact::evaluate(&matches, &p, &model, None)
            && i.rounds >= 15
        {
            wpas.push(i.wpa_per_round);
        }
    }
    let n = wpas.len() as f32;
    let mean = wpas.iter().sum::<f32>() / n;
    let sd = (wpas.iter().map(|w| (w - mean).powi(2)).sum::<f32>() / (n - 1.0)).sqrt();
    let mut sorted = wpas.clone();
    sorted.sort_by(f32::total_cmp);
    println!("players {} shrunk wpa/rd mean {mean:+.4} sd {sd:.4} p10 {:+.4} p90 {:+.4}; model-scaled rating sd ≈ {:.3}", n, sorted[(n * 0.1) as usize], sorted[(n * 0.9) as usize], 0.15 * sd / 0.03);
}
