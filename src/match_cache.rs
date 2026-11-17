//! On-disk cache of parsed full matches, keyed by match id.
//!
//! Every match in a lobby is shared by ten players, and a player's history overlaps with their
//! duo's, so caching turns the deep fetch from "N players × M matches" into a few hundred
//! kilobytes read from disk. Each file is one `MatchDto` as JSON (~0.5 MB); the index maps
//! match id → (start time, act, players) so a player's cached history can be assembled without
//! opening every file.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::impact::MatchDto;

/// Hard cap on files kept, oldest first. An act can span a season boundary, so age alone
/// must not discard matches the current-act rating still needs.
const MAX_FILES: usize = 1_500;

#[derive(Serialize, Deserialize, Clone)]
pub struct Entry {
    pub started_at: String,
    pub act: Option<String>,
    pub players: Vec<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct Index {
    entries: HashMap<String, Entry>,
    /// Proven contiguous API pages per player; incidental shared-lobby matches are not a prefix.
    #[serde(default)]
    prefixes: HashMap<String, Prefix>,
}

#[derive(Serialize, Deserialize, Default)]
struct Prefix {
    ids: Vec<String>,
    exhausted: bool,
}

pub struct MatchCache {
    dir: Option<PathBuf>,
    index: Index,
    /// Parsed matches kept in memory for the session, so lobby-mates share one parse.
    loaded: HashMap<String, Arc<MatchDto>>,
}

impl MatchCache {
    pub fn load() -> Self {
        Self::load_from(crate::config::local_data_dir().map(|d| d.join("matches")))
    }

    fn load_from(dir: Option<PathBuf>) -> Self {
        let saved = dir
            .as_ref()
            .and_then(|d| std::fs::read(d.join("index.json")).ok())
            .and_then(|b| serde_json::from_slice::<Index>(&b).ok());
        let recovering = saved.is_none();
        let mut index = saved.unwrap_or_else(|| dir.as_ref().map_or_else(Index::default, |d| recover(d)));
        let mut changed = recovering;
        if let Some(dir) = &dir {
            changed |= prune(dir, &mut index);
        }
        let cache = Self { dir, index, loaded: HashMap::new() };
        if changed {
            cache.save_index();
        }
        cache
    }


    #[cfg(test)]
    /// Match ids this player appears in, newest first.
    pub fn history(&self, puuid: &str) -> Vec<(String, Entry)> {
        let mut ids: Vec<(String, Entry)> = self
            .index
            .entries
            .iter()
            .filter(|(_, e)| e.players.iter().any(|p| p == puuid))
            .map(|(id, e)| (id.clone(), e.clone()))
            .collect();
        ids.sort_by(|a, b| b.1.started_at.cmp(&a.1.started_at));
        ids
    }

    pub fn prefix(&mut self, puuid: &str) -> (Vec<Arc<MatchDto>>, bool) {
        let Some(prefix) = self.index.prefixes.get(puuid) else { return (Vec::new(), false) };
        let (ids, exhausted) = (prefix.ids.clone(), prefix.exhausted);
        let matches: Vec<_> = ids.iter().map_while(|id| self.get(id)).collect();
        let complete = exhausted && matches.len() == ids.len();
        (matches, complete)
    }

    pub fn set_prefix(&mut self, puuid: &str, matches: &[Arc<MatchDto>], exhausted: bool) {
        self.index.prefixes.insert(
            puuid.to_owned(),
            Prefix { ids: matches.iter().map(|m| m.id().to_owned()).collect(), exhausted },
        );
        self.save_index();
    }

    /// Reads one cached match (from memory if already parsed this session).
    pub fn get(&mut self, id: &str) -> Option<Arc<MatchDto>> {
        if let Some(m) = self.loaded.get(id) {
            return Some(m.clone());
        }
        let bytes = std::fs::read(self.dir.as_ref()?.join(format!("{id}.json"))).ok()?;
        let m: MatchDto = serde_json::from_slice(&bytes).ok()?;
        let m = Arc::new(m);
        self.loaded.insert(id.to_owned(), m.clone());
        Some(m)
    }

    /// Stores freshly fetched matches. Disk failures are ignored: the session still has them.
    pub fn insert(&mut self, matches: Vec<MatchDto>) -> Vec<Arc<MatchDto>> {
        let mut out = Vec::with_capacity(matches.len());
        let mut index_changed = false;
        for m in matches {
            let id = m.id().to_owned();
            if !self.index.entries.contains_key(&id) {
                let entry = entry(&m);
                if let Some(dir) = &self.dir {
                    let _ = std::fs::create_dir_all(dir);
                    if let Ok(bytes) = serde_json::to_vec(&m)
                        && std::fs::write(dir.join(format!("{id}.json")), bytes).is_ok()
                    {
                        self.index.entries.insert(id.clone(), entry);
                        index_changed = true;
                    }
                } else {
                    self.index.entries.insert(id.clone(), entry);
                    index_changed = true;
                }
            }
            let m = Arc::new(m);
            self.loaded.insert(id, m.clone());
            out.push(m);
        }
        if index_changed {
            self.save_index();
        }
        out
    }

    fn save_index(&self) {
        if let Some(dir) = &self.dir {
            let _ = std::fs::create_dir_all(dir);
            if let Ok(bytes) = serde_json::to_vec(&self.index) {
                let tmp = dir.join("index.tmp");
                if std::fs::write(&tmp, bytes).is_ok() {
                    let _ = std::fs::rename(tmp, dir.join("index.json"));
                }
            }
        }
    }
}

fn entry(m: &MatchDto) -> Entry {
    Entry { started_at: m.started_at().to_owned(), act: m.act(), players: m.player_ids() }
}

/// A missing/corrupt index must not destroy valid cached matches.
fn recover(dir: &std::path::Path) -> Index {
    let mut index = Index::default();
    if let Ok(files) = std::fs::read_dir(dir) {
        for file in files.flatten() {
            let name = file.file_name();
            let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json")).filter(|id| *id != "index") else {
                continue;
            };
            if let Some(m) = std::fs::read(file.path())
                .ok()
                .and_then(|bytes| serde_json::from_slice::<MatchDto>(&bytes).ok())
                .filter(|m| m.id() == id)
            {
                index.entries.insert(id.to_owned(), entry(&m));
            }
        }
    }
    index
}

/// Drops entries beyond `MAX_FILES` or whose file is missing.
fn prune(dir: &std::path::Path, index: &mut Index) -> bool {
    let present: HashSet<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter_map(|n| n.strip_suffix(".json").filter(|id| *id != "index").map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut ids: Vec<(String, String)> =
        index.entries.iter().map(|(id, e)| (id.clone(), e.started_at.clone())).collect();
    ids.sort_by(|a, b| b.1.cmp(&a.1));
    let mut remove = Vec::new();
    for (i, (id, _)) in ids.iter().enumerate() {
        if i >= MAX_FILES || !present.contains(id) {
            remove.push(id.clone());
        }
    }
    for id in &remove {
        index.entries.remove(id);
        let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
    }
    for prefix in index.prefixes.values_mut() {
        if let Some(i) = prefix.ids.iter().position(|id| !index.entries.contains_key(id)) {
            prefix.ids.truncate(i);
            prefix.exhausted = false;
        }
    }
    // Stray match files can be recovered if the index is lost; keep them.
    !remove.is_empty()
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_newest_first_and_filtered_by_player() {
        let mut cache = MatchCache { dir: None, index: Index::default(), loaded: HashMap::new() };
        let entry = |t: &str, players: &[&str]| Entry {
            started_at: t.into(),
            act: None,
            players: players.iter().map(|p| p.to_string()).collect(),
        };
        cache.index.entries.insert("old".into(), entry("2026-01-01T00:00:00Z", &["a", "b"]));
        cache.index.entries.insert("new".into(), entry("2026-03-01T00:00:00Z", &["a"]));
        cache.index.entries.insert("other".into(), entry("2026-02-01T00:00:00Z", &["b"]));
        let ids: Vec<String> = cache.history("a").into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, ["new", "old"]);
        assert!(cache.get("new").is_none(), "no directory: nothing to read");
    }

    #[test]
    fn cached_match_survives_reloads_and_corrupt_index() {
        let dir = std::env::temp_dir().join(format!(
            "vltracker-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let m: MatchDto = serde_json::from_str(
            r#"{"metadata":{"match_id":"one","started_at":"2026-01-01T00:00:00Z","season":{"id":"act"}},"players":[{"puuid":"a"}]}"#
        ).unwrap();
        let mut cache = MatchCache::load_from(Some(dir.clone()));
        let prefix = cache.insert(vec![m]);
        let stray: MatchDto = serde_json::from_str(
            r#"{"metadata":{"match_id":"stray","started_at":"2026-02-01T00:00:00Z"},"players":[{"puuid":"a"}]}"#
        ).unwrap();
        cache.insert(vec![stray]);
        cache.set_prefix("a", &prefix, false);
        for _ in 0..2 {
            cache = MatchCache::load_from(Some(dir.clone()));
            assert_eq!(cache.history("a").len(), 2);
            let (pages, done) = cache.prefix("a");
            assert!(!done && pages.len() == 1 && pages[0].id() == "one");
            assert_eq!(cache.get("one").unwrap().act().as_deref(), Some("act"));
            assert!(dir.join("index.json").exists());
        }
        std::fs::write(dir.join("index.json"), b"broken").unwrap();
        cache = MatchCache::load_from(Some(dir.clone()));
        assert_eq!(cache.history("a").len(), 2);
        assert!(cache.get("one").is_some());
        assert!(cache.prefix("a").0.is_empty(), "corrupt index cannot prove contiguous history");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
