use crate::db::{path_hash, XerxesDb};
use anyhow::Result;
use jwalk::WalkDir;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const IGNORED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    ".cache",
    "Trash",
    "proc",
    "sys",
    "dev",
    "tmp",
    "target",
    "dist",
    "build",
    "venv",
    "__pycache__",
    "Library",
];

const BATCH_SIZE: usize = 5000;

fn is_ignored_name(name: &str) -> bool {
    name.starts_with('.') || IGNORED_DIRS.contains(&name)
}

/// True if `path` is outside every root, or has a hidden/ignored component below its root.
pub fn is_ignored(path: &Path, roots: &[PathBuf]) -> bool {
    let Some(rest) = roots.iter().find_map(|r| path.strip_prefix(r).ok()) else {
        return true;
    };
    rest.components()
        .any(|c| is_ignored_name(&c.as_os_str().to_string_lossy()))
}

fn scan(root: &Path, db: &XerxesDb, seen: &mut HashSet<u64>) -> Result<()> {
    let walk = WalkDir::new(root).skip_hidden(true).process_read_dir(
        |_depth, _path, _read_dir_state, children| {
            children.retain(|dir_entry_result| {
                dir_entry_result.as_ref().is_ok_and(|e| {
                    e.file_type().is_dir() && !is_ignored_name(&e.file_name().to_string_lossy())
                })
            });
        },
    );

    let mut batch = Vec::with_capacity(BATCH_SIZE);
    for dir_entry in walk.into_iter().flatten() {
        let path = dir_entry.path();
        seen.insert(path_hash(&path));
        batch.push(path);
        if batch.len() == BATCH_SIZE {
            db.add_directories(&batch)?;
            batch.clear();
        }
    }
    db.add_directories(&batch)?;
    Ok(())
}

/// Indexes every root, then drops entries that vanished or are now ignored.
pub fn reindex(roots: &[PathBuf], db: &XerxesDb) -> Result<()> {
    let mut seen = HashSet::new();
    for root in roots {
        scan(root, db, &mut seen)?;
    }
    // Only entries the walk didn't reach need a stat (e.g. dirs added by jumping to them).
    db.retain_directories(|hash, path| {
        seen.contains(&hash) || (path().is_dir() && !is_ignored(&path(), roots))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_rules() {
        let roots = [PathBuf::from("/home/u"), PathBuf::from("/mnt/tmp")];
        assert!(!is_ignored(Path::new("/home/u"), &roots));
        assert!(!is_ignored(Path::new("/home/u/code/app"), &roots));
        assert!(is_ignored(
            Path::new("/home/u/code/app/target/debug"),
            &roots
        ));
        assert!(is_ignored(Path::new("/home/u/.config/x"), &roots));
        assert!(is_ignored(Path::new("/elsewhere"), &roots));
        // Ignored names in the root itself don't count.
        assert!(!is_ignored(Path::new("/mnt/tmp/photos"), &roots));
    }

    #[test]
    fn reindex_adds_and_prunes() {
        let base = std::env::temp_dir().join(format!("xerxes-reindex-{}", std::process::id()));
        let root = base.join("root");
        for d in ["a/b", "a/target/x", ".hidden/y", "gone"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let db = XerxesDb::new(base.join("db.redb")).unwrap();
        let roots = [root.clone()];

        reindex(&roots, &db).unwrap();
        // A stale entry inside an ignored dir, as the old watcher used to add.
        db.add_directories(&[root.join("a/target/x")]).unwrap();
        std::fs::remove_dir(root.join("gone")).unwrap();
        reindex(&roots, &db).unwrap();

        let dirs = db.get_all_directories().unwrap();
        let mut paths: Vec<_> = dirs.iter().map(|d| d.path.clone()).collect();
        paths.sort();
        let r = root.to_string_lossy();
        assert_eq!(paths, [r.to_string(), format!("{r}/a"), format!("{r}/a/b")]);
        std::fs::remove_dir_all(base).unwrap();
    }
}
