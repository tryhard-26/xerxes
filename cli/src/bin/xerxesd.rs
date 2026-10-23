use anyhow::{Context, Result};
use directories::{ProjectDirs, UserDirs};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use tracing::{error, info};

use xerxes_core::db::XerxesDb;
use xerxes_core::index;
use xerxes_core::ipc::{Request, Response};
use xerxes_core::query;

fn get_db_path() -> Result<PathBuf> {
    let proj_dirs = ProjectDirs::from("com", "xerxes", "xerxes")
        .context("Could not find project directories")?;
    let data_dir = proj_dirs.data_dir();
    std::fs::create_dir_all(data_dir)?;
    Ok(data_dir.join("xerxes.redb"))
}

fn get_socket_path() -> Result<PathBuf> {
    let proj_dirs = ProjectDirs::from("com", "xerxes", "xerxes")
        .context("Could not find project directories")?;
    let run_dir = proj_dirs
        .runtime_dir()
        .unwrap_or_else(|| proj_dirs.data_dir());
    std::fs::create_dir_all(run_dir)?;
    Ok(run_dir.join("xerxesd.sock"))
}

/// $HOME plus any absolute paths in the colon-separated XERXES_ROOTS env var.
fn index_roots() -> Vec<PathBuf> {
    let extra = std::env::var_os("XERXES_ROOTS").unwrap_or_default();
    UserDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .into_iter()
        .chain(std::env::split_paths(&extra).filter(|p| p.is_absolute()))
        // Resolve symlinked roots: the walker won't descend into them otherwise.
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .collect()
}

fn index_all(db: &XerxesDb) -> Result<()> {
    index::reindex(&index_roots(), db)
}

fn status<T>(res: Result<T>, msg: &str) -> Response {
    match res {
        Ok(_) => Response::Status(msg.to_string()),
        Err(e) => Response::Error(e.to_string()),
    }
}

fn handle_client(mut stream: UnixStream, db: Arc<XerxesDb>) -> Result<()> {
    let mut buffer = Vec::new();
    stream.read_to_end(&mut buffer)?;

    let request: Request = bincode::deserialize(&buffer)?;
    info!("Received request: {:?}", request);

    let response = match request {
        Request::Query(q) => match query::search(&db, &q) {
            Ok(results) => {
                let paths = results.into_iter().map(|r| (r.path, r.score)).collect();
                Response::QueryResults(paths)
            }
            Err(e) => Response::Error(e.to_string()),
        },
        Request::Select(p) => {
            let path = PathBuf::from(p);
            // Jumps to unindexed dirs (outside roots, hidden, ...) make them searchable.
            let res = match db.contains(&path) {
                Ok(true) => Ok(0),
                _ => db.add_directories(&[&path]),
            };
            status(res.and_then(|_| db.update_dir_access(&path)), "Ok")
        }
        Request::AddAlias { nickname, path } => {
            let msg = format!("Alias added: {} -> {}", nickname, path);
            status(db.add_alias(&nickname, &PathBuf::from(path)), &msg)
        }
        Request::RecordSelection { query, path } => status(
            db.record_selection(&query, &PathBuf::from(path)),
            "Selection recorded",
        ),
        Request::ListAliases => match db.list_aliases() {
            Ok(aliases) => Response::AliasList(aliases),
            Err(e) => Response::Error(e.to_string()),
        },
        Request::RemoveAlias(nickname) => match db.remove_alias(&nickname) {
            Ok(true) => Response::Status(format!("Alias removed: {}", nickname)),
            Ok(false) => Response::Error(format!("No such alias: {}", nickname)),
            Err(e) => Response::Error(e.to_string()),
        },
        Request::ClearAliases => status(db.clear_aliases(), "All aliases cleared"),
        Request::ClearSelectionMemory => {
            status(db.clear_selection_memory(), "Smart learning memory cleared")
        }
        Request::Reindex => status(index_all(&db), "Reindex complete"),
        Request::Stats => match db.get_stats() {
            Ok((dirs, segments)) => Response::Status(format!(
                "Indexed: {} directories, {} unique segments",
                dirs, segments
            )),
            Err(e) => Response::Error(e.to_string()),
        },
        Request::Doctor => Response::Status("Doctor says you're fine".to_string()),
    };

    let response_bytes = bincode::serialize(&response)?;
    stream.write_all(&response_bytes)?;

    Ok(())
}

fn start_watcher(db: Arc<XerxesDb>) {
    thread::spawn(move || {
        use notify::event::ModifyKind;
        use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
        use std::sync::mpsc::channel;

        let roots = index_roots();
        let (tx, rx) = channel();
        let mut watcher = match RecommendedWatcher::new(tx, Config::default()) {
            Ok(w) => w,
            Err(e) => {
                error!("Failed to create watcher: {}", e);
                return;
            }
        };

        for root in &roots {
            info!("Starting live filesystem watcher on {:?}", root);
            if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
                error!("Failed to watch {:?}: {}", root, e);
            }
        }

        // Drain events in bursts so e.g. a `git clone` costs one transaction, not thousands.
        while let Ok(first) = rx.recv() {
            let (mut add, mut remove) = (Vec::new(), Vec::new());
            for res in std::iter::once(first).chain(rx.try_iter()) {
                let event: notify::Event = match res {
                    Ok(event) => event,
                    Err(e) => {
                        error!("watch error: {:?}", e);
                        continue;
                    }
                };
                if !matches!(
                    event.kind,
                    EventKind::Create(_)
                        | EventKind::Remove(_)
                        | EventKind::Modify(ModifyKind::Name(_))
                ) {
                    continue;
                }
                for path in event.paths {
                    if path.is_dir() {
                        if !index::is_ignored(&path, &roots) {
                            add.push(path);
                        }
                    } else {
                        remove.push(path);
                    }
                }
            }
            if let Err(e) = db.add_directories(&add) {
                error!("Failed to index new directories: {}", e);
            }
            if let Err(e) = db.remove_directories(&remove) {
                error!("Failed to drop removed directories: {}", e);
            }
        }
    });
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    info!("Starting xerxesd...");

    // Opening the DB first means a second daemon fails on redb's file lock
    // before it can delete the live daemon's socket.
    let db_path = get_db_path()?;
    let db = Arc::new(XerxesDb::new(&db_path)?);
    info!("Opened DB at {:?}", db_path);

    // Bind before the slow work below so clients don't time out waiting.
    let socket_path = get_socket_path()?;
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }
    let listener = UnixListener::bind(&socket_path)?;
    info!("Listening on {:?}", socket_path);

    {
        let db = Arc::clone(&db);
        thread::spawn(move || {
            let res = if db.is_empty().unwrap_or(false) {
                info!("Database is empty, starting initial reindex...");
                index_all(&db)
            } else {
                Ok(())
            };
            match res.and_then(|_| db.get_all_directories()) {
                Ok(_) => info!("Directory cache warmed."),
                Err(e) => error!("Startup indexing failed: {}", e),
            }
        });
    }

    start_watcher(Arc::clone(&db));

    for stream_result in listener.incoming() {
        match stream_result {
            Ok(stream) => {
                let db_clone = Arc::clone(&db);
                thread::spawn(move || {
                    if let Err(e) = handle_client(stream, db_clone) {
                        error!("Error handling client: {}", e);
                    }
                });
            }
            Err(e) => error!("Connection error: {}", e),
        }
    }

    Ok(())
}
