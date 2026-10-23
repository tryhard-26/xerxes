use crate::db::XerxesDb;
use anyhow::Result;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};
use std::path::Path;

const MAX_RESULTS: usize = 20;

#[derive(Debug)]
pub struct SearchResult {
    pub path: String,
    pub score: f32,
}

fn pinned(path: &Path) -> Vec<SearchResult> {
    vec![SearchResult {
        path: path.to_string_lossy().into_owned(),
        score: f32::MAX,
    }]
}

pub fn search(db: &XerxesDb, query: &str) -> Result<Vec<SearchResult>> {
    if let Ok(Some(path)) = db.get_alias(query) {
        return Ok(pinned(&path));
    }

    if let Ok(Some(path)) = db.get_selection_memory(query) {
        if path.is_dir() {
            return Ok(pinned(&path));
        }
    }

    let dirs = db.get_all_directories()?;
    let frecencies = db.frecencies();
    let query_lower = query.to_lowercase();
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::default();
    let mut buf = Vec::new();

    let mut scored: Vec<(f32, usize)> = dirs
        .iter()
        .enumerate()
        .filter_map(|(i, dir)| {
            let fuzzy = pattern.score(Utf32Str::new(&dir.path, &mut buf), &mut matcher)?;
            let bonus = if dir.name == query_lower {
                20000.0
            } else if dir.name.starts_with(&query_lower) {
                10000.0
            } else if dir.name.contains(&query_lower) {
                5000.0
            } else {
                0.0
            };
            let frecency = frecencies.get(&dir.hash).copied().unwrap_or(0.0);
            Some((fuzzy as f32 + bonus + frecency * 200.0, i))
        })
        .collect();

    let by_score = |a: &(f32, usize), b: &(f32, usize)| b.0.total_cmp(&a.0);
    if scored.len() > MAX_RESULTS {
        scored.select_nth_unstable_by(MAX_RESULTS, by_score);
        scored.truncate(MAX_RESULTS);
    }
    scored.sort_unstable_by(by_score);

    Ok(scored
        .into_iter()
        .map(|(score, i)| SearchResult {
            path: dirs[i].path.clone(),
            score,
        })
        // The index can lag behind deletions the watcher missed.
        .filter(|r| Path::new(&r.path).is_dir())
        .collect())
}
