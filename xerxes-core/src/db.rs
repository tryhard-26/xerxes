use anyhow::Result;
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const SEGMENT_ID_TO_STR: TableDefinition<u32, &str> = TableDefinition::new("segment_id_to_str");
const SEGMENT_STR_TO_ID: TableDefinition<&str, u32> = TableDefinition::new("segment_str_to_id");
const DIRECTORIES: TableDefinition<u64, &[u8]> = TableDefinition::new("directories");
const METADATA: TableDefinition<u64, &[u8]> = TableDefinition::new("metadata");
const STATE: TableDefinition<&str, u32> = TableDefinition::new("state");
const ALIASES: TableDefinition<&str, &str> = TableDefinition::new("aliases");
const SELECTION_MEMORY: TableDefinition<&str, &str> = TableDefinition::new("selection_memory");

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DirectoryEntry {
    pub segments: Vec<u32>,
    pub depth: u16,
    pub hash: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct DirMetadata {
    pub frecency: f32,
    pub last_access: u64,
    pub access_count: u32,
}

impl DirMetadata {
    /// Frecency with a 7-day half-life applied up to `now`.
    pub fn frecency_at(&self, now: u64) -> f32 {
        if self.last_access == 0 {
            return self.frecency;
        }
        let elapsed_days = now.saturating_sub(self.last_access) as f32 / 86400.0;
        self.frecency * 0.5f32.powf(elapsed_days / 7.0)
    }
}

/// A directory as the query loop wants it: resolved once, not per query.
pub struct CachedDir {
    pub path: String,
    /// Lowercased last path segment.
    pub name: String,
    pub hash: u64,
}

#[derive(Default)]
struct Segments {
    by_str: HashMap<String, u32>,
    by_id: HashMap<u32, String>,
    next: u32,
}

impl Segments {
    fn resolve(&self, ids: &[u32]) -> PathBuf {
        ids.iter().filter_map(|id| self.by_id.get(id)).collect()
    }
}

// ponytail: DefaultHasher output isn't guaranteed stable across Rust releases; switch to a
// fixed hash (e.g. FNV) plus a one-time rehash migration if a toolchain bump ever orphans rows.
pub fn path_hash(path: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct XerxesDb {
    db: Database,
    segments: Mutex<Segments>,
    /// Only directories that have ever been visited; absent means default.
    meta: Mutex<HashMap<u64, DirMetadata>>,
    dir_cache: Mutex<Option<Arc<Vec<CachedDir>>>>,
    cache_gen: AtomicU64,
}

impl XerxesDb {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let db = Database::create(path)?;
        let mut segments = Segments::default();
        let mut meta = HashMap::new();

        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(SEGMENT_STR_TO_ID)?;
            write_txn.open_table(DIRECTORIES)?;
            write_txn.open_table(ALIASES)?;
            write_txn.open_table(SELECTION_MEMORY)?;
            segments.next = write_txn
                .open_table(STATE)?
                .get("next_segment_id")?
                .map(|v| v.value())
                .unwrap_or(0);

            for item in write_txn.open_table(SEGMENT_ID_TO_STR)?.iter()? {
                let (k, v) = item?;
                segments.by_str.insert(v.value().to_string(), k.value());
                segments.by_id.insert(k.value(), v.value().to_string());
            }

            for item in write_txn.open_table(METADATA)?.iter()? {
                let (k, v) = item?;
                if let Ok(m) = bincode::deserialize::<DirMetadata>(v.value()) {
                    if m.access_count > 0 {
                        meta.insert(k.value(), m);
                    }
                }
            }
        }
        write_txn.commit()?;

        Ok(Self {
            db,
            segments: Mutex::new(segments),
            meta: Mutex::new(meta),
            dir_cache: Mutex::new(None),
            cache_gen: AtomicU64::new(0),
        })
    }

    pub fn invalidate_cache(&self) {
        let mut cache = self.dir_cache.lock().unwrap();
        self.cache_gen.fetch_add(1, Ordering::SeqCst);
        *cache = None;
    }

    /// Inserts paths not already indexed, in one transaction. Returns how many were new.
    pub fn add_directories<P: AsRef<Path>>(&self, paths: &[P]) -> Result<usize> {
        let txn = self.db.begin_write()?;
        let mut segs = self.segments.lock().unwrap();
        // New segments live here until commit so a failed txn can't leave dangling ids in memory.
        let mut pending: HashMap<String, u32> = HashMap::new();
        let mut next = segs.next;
        let mut added = 0;
        {
            let mut id_to_str = txn.open_table(SEGMENT_ID_TO_STR)?;
            let mut str_to_id = txn.open_table(SEGMENT_STR_TO_ID)?;
            let mut dirs = txn.open_table(DIRECTORIES)?;

            for path in paths {
                let path = path.as_ref();
                let hash = path_hash(path);
                if dirs.get(hash)?.is_some() {
                    continue;
                }

                let mut ids = Vec::new();
                for component in path.components() {
                    let s = component.as_os_str().to_string_lossy();
                    let id = match segs.by_str.get(s.as_ref()).or(pending.get(s.as_ref())) {
                        Some(&id) => id,
                        None => {
                            let id = next;
                            next += 1;
                            id_to_str.insert(id, s.as_ref())?;
                            str_to_id.insert(s.as_ref(), id)?;
                            pending.insert(s.into_owned(), id);
                            id
                        }
                    };
                    ids.push(id);
                }

                let entry = DirectoryEntry {
                    depth: ids.len() as u16,
                    segments: ids,
                    hash,
                };
                dirs.insert(hash, bincode::serialize(&entry)?.as_slice())?;
                added += 1;
            }

            if next != segs.next {
                txn.open_table(STATE)?.insert("next_segment_id", next)?;
            }
        }

        if added == 0 {
            txn.abort()?;
            return Ok(0);
        }
        txn.commit()?;

        segs.next = next;
        for (s, id) in pending {
            segs.by_id.insert(id, s.clone());
            segs.by_str.insert(s, id);
        }
        drop(segs);
        self.invalidate_cache();
        Ok(added)
    }

    pub fn contains(&self, path: &Path) -> Result<bool> {
        let read_txn = self.db.begin_read()?;
        Ok(read_txn
            .open_table(DIRECTORIES)?
            .get(path_hash(path))?
            .is_some())
    }

    /// Removes indexed paths. Returns how many were actually indexed.
    pub fn remove_directories<P: AsRef<Path>>(&self, paths: &[P]) -> Result<usize> {
        let txn = self.db.begin_write()?;
        let mut removed = 0;
        {
            let mut dirs = txn.open_table(DIRECTORIES)?;
            for path in paths {
                if dirs.remove(path_hash(path.as_ref()))?.is_some() {
                    removed += 1;
                }
            }
        }
        if removed == 0 {
            txn.abort()?;
            return Ok(0);
        }
        txn.commit()?;
        self.invalidate_cache();
        Ok(removed)
    }

    /// Drops every indexed directory for which `keep(hash, path)` returns false.
    /// `path` resolves lazily so callers that decide by hash skip the work.
    pub fn retain_directories(
        &self,
        mut keep: impl FnMut(u64, &dyn Fn() -> PathBuf) -> bool,
    ) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let segs = self.segments.lock().unwrap();
            txn.open_table(DIRECTORIES)?.retain(|hash, v| {
                let path = || {
                    bincode::deserialize::<DirectoryEntry>(v)
                        .map(|e| segs.resolve(&e.segments))
                        .unwrap_or_default()
                };
                keep(hash, &path)
            })?;
        }
        txn.commit()?;
        self.invalidate_cache();
        Ok(())
    }

    pub fn get_all_directories(&self) -> Result<Arc<Vec<CachedDir>>> {
        if let Some(cache) = self.dir_cache.lock().unwrap().as_ref() {
            return Ok(Arc::clone(cache));
        }

        let gen = self.cache_gen.load(Ordering::SeqCst);
        let read_txn = self.db.begin_read()?;
        let dirs = read_txn.open_table(DIRECTORIES)?;

        let mut entries = Vec::with_capacity(dirs.len()? as usize);
        {
            let segs = self.segments.lock().unwrap();
            for item in dirs.iter()? {
                let (k, v) = item?;
                let entry: DirectoryEntry = bincode::deserialize(v.value())?;
                let path = segs.resolve(&entry.segments);
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase();
                entries.push(CachedDir {
                    path: path.to_string_lossy().into_owned(),
                    name,
                    hash: k.value(),
                });
            }
        }

        let entries = Arc::new(entries);
        let mut cache = self.dir_cache.lock().unwrap();
        // Skip storing if a write landed while we were reading; it would be stale.
        if self.cache_gen.load(Ordering::SeqCst) == gen {
            *cache = Some(Arc::clone(&entries));
        }
        Ok(entries)
    }

    /// Decayed frecency for every visited directory, keyed by path hash.
    pub fn frecencies(&self) -> HashMap<u64, f32> {
        let now = now_secs();
        self.meta
            .lock()
            .unwrap()
            .iter()
            .map(|(&h, m)| (h, m.frecency_at(now)))
            .collect()
    }

    pub fn update_dir_access(&self, path: &Path) -> Result<()> {
        let hash = path_hash(path);
        let now = now_secs();
        let mut meta = self.meta.lock().unwrap();
        let m = meta.entry(hash).or_default();
        m.frecency = m.frecency_at(now) + 1.0;
        m.last_access = now;
        m.access_count += 1;
        let meta_bytes = bincode::serialize(m)?;

        let write_txn = self.db.begin_write()?;
        write_txn
            .open_table(METADATA)?
            .insert(hash, meta_bytes.as_slice())?;
        write_txn.commit()?;
        Ok(())
    }

    pub fn get_stats(&self) -> Result<(u64, usize)> {
        let read_txn = self.db.begin_read()?;
        let dir_count = read_txn.open_table(DIRECTORIES)?.len()?;
        let segment_count = self.segments.lock().unwrap().by_id.len();
        Ok((dir_count, segment_count))
    }

    pub fn is_empty(&self) -> Result<bool> {
        let read_txn = self.db.begin_read()?;
        Ok(read_txn.open_table(DIRECTORIES)?.is_empty()?)
    }

    fn put(&self, table: TableDefinition<&str, &str>, key: &str, value: &str) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        write_txn.open_table(table)?.insert(key, value)?;
        write_txn.commit()?;
        Ok(())
    }

    fn get(&self, table: TableDefinition<&str, &str>, key: &str) -> Result<Option<PathBuf>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(table)?;
        let value = table.get(key)?.map(|v| PathBuf::from(v.value()));
        Ok(value)
    }

    fn clear(&self, table: TableDefinition<&str, &str>) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        write_txn.open_table(table)?.retain(|_, _| false)?;
        write_txn.commit()?;
        Ok(())
    }

    pub fn add_alias(&self, nickname: &str, path: &Path) -> Result<()> {
        self.put(ALIASES, nickname, &path.to_string_lossy())
    }

    pub fn get_alias(&self, nickname: &str) -> Result<Option<PathBuf>> {
        self.get(ALIASES, nickname)
    }

    pub fn record_selection(&self, query: &str, path: &Path) -> Result<()> {
        self.put(SELECTION_MEMORY, query, &path.to_string_lossy())
    }

    pub fn get_selection_memory(&self, query: &str) -> Result<Option<PathBuf>> {
        self.get(SELECTION_MEMORY, query)
    }

    pub fn list_aliases(&self) -> Result<Vec<(String, String)>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ALIASES)?;
        let mut results = Vec::new();
        for item in table.iter()? {
            let (k, v) = item?;
            results.push((k.value().to_string(), v.value().to_string()));
        }
        Ok(results)
    }

    /// Returns whether the alias existed.
    pub fn remove_alias(&self, nickname: &str) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let existed = write_txn.open_table(ALIASES)?.remove(nickname)?.is_some();
        write_txn.commit()?;
        Ok(existed)
    }

    pub fn clear_aliases(&self) -> Result<()> {
        self.clear(ALIASES)
    }

    pub fn clear_selection_memory(&self) -> Result<()> {
        self.clear(SELECTION_MEMORY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_alias_and_frecency_roundtrip() {
        let dir = std::env::temp_dir().join(format!("xerxes-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        let db = XerxesDb::new(&dir).unwrap();

        assert_eq!(db.add_directories(&["/a/b", "/a/c"]).unwrap(), 2);
        assert_eq!(db.add_directories(&["/a/b"]).unwrap(), 0);
        let dirs = db.get_all_directories().unwrap();
        let mut paths: Vec<_> = dirs.iter().map(|d| d.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["/a/b", "/a/c"]);

        // Cache must be invalidated by writes.
        db.remove_directories(&["/a/b"]).unwrap();
        assert_eq!(db.get_all_directories().unwrap().len(), 1);
        db.retain_directories(|_, p| p() != Path::new("/a/c"))
            .unwrap();
        assert!(db.is_empty().unwrap());

        db.add_alias("proj", Path::new("/a/c")).unwrap();
        assert_eq!(db.get_alias("proj").unwrap(), Some(PathBuf::from("/a/c")));
        assert!(db.remove_alias("proj").unwrap());
        assert!(!db.remove_alias("proj").unwrap());

        db.update_dir_access(Path::new("/a/c")).unwrap();
        db.update_dir_access(Path::new("/a/c")).unwrap();
        let f = db.frecencies()[&path_hash(Path::new("/a/c"))];
        assert!(f > 1.9 && f <= 2.0, "{f}");

        drop(db);
        let db = XerxesDb::new(&dir).unwrap();
        assert!(db.frecencies()[&path_hash(Path::new("/a/c"))] > 1.9);
        std::fs::remove_file(&dir).unwrap();
    }
}
