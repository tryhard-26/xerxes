use crate::db::XerxesDb;
use anyhow::Result;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::Matcher;
use std::path::PathBuf;

#[derive(Debug)]
pub struct SearchResult {
    pub path: PathBuf,
    pub score: f32,
    pub frecency: f32,
}

pub fn search(db: &XerxesDb, query: &str) -> Result<Vec<SearchResult>> {
    if let Ok(Some(path)) = db.get_alias(query) {
        return Ok(vec![SearchResult {
            path,
            score: f32::MAX,
            frecency: 0.0,
        }]);
    }

    if let Ok(Some(path)) = db.get_selection_memory(query) {
        if path.exists() {
            return Ok(vec![SearchResult {
                path,
                score: f32::MAX,
                frecency: 0.0,
            }]);
        }
    }

    let dirs = db.get_all_directories()?;
    let mut matcher = Matcher::default();

    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);

    let mut results = Vec::new();
    let mut buf = Vec::new();

    for (dir, meta) in dirs.as_ref() {
        let path = db.resolve_path(&dir.segments);
        let path_str = path.to_string_lossy();

        let mut exact_match_bonus = 0.0;
        let last_segment = path.file_name().unwrap_or_default().to_string_lossy();

        if last_segment.eq_ignore_ascii_case(query) {
            exact_match_bonus = 20000.0;
        } else if last_segment
            .to_lowercase()
            .starts_with(&query.to_lowercase())
        {
            exact_match_bonus = 10000.0;
        } else if last_segment.to_lowercase().contains(&query.to_lowercase()) {
            exact_match_bonus = 5000.0;
        }

        let fuzzy_score = pattern.score(
            nucleo_matcher::Utf32Str::new(&path_str, &mut buf),
            &mut matcher,
        );

        if let Some(s) = fuzzy_score {
            let score = (s as f32) + exact_match_bonus + (meta.frecency * 200.0);

            results.push(SearchResult {
                path,
                score,
                frecency: meta.frecency,
            });
        }
    }

    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());

    // Hide missing dirs (deleted, or on an unplugged drive) but keep them indexed.
    Ok(results
        .into_iter()
        .filter(|r| r.path.is_dir())
        .take(20)
        .collect())
}
