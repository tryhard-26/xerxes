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
        .collect()
}

fn index_all(db: &XerxesDb) -> Result<()> {
    index_roots()
        .iter()
        .try_for_each(|root| index::scan_and_index(root, db))
}

fn handle_client(mut stream: UnixStream, db: Arc<XerxesDb>) -> Result<()> {
    let mut buffer = Vec::new();
    stream.read_to_end(&mut buffer)?;

    let request: Request = bincode::deserialize(&buffer)?;
    info!("Received request: {:?}", request);

    let response = match request {
        Request::Query(q) => match query::search(&db, &q) {
            Ok(results) => {
                let paths = results
                    .into_iter()
                    .map(|r| (r.path.to_string_lossy().into_owned(), r.score))
                    .collect();
                Response::QueryResults(paths)
            }
            Err(e) => Response::Error(e.to_string()),
        },
        Request::Select(p) => {
            let path = PathBuf::from(p);
            match db.update_dir_access(&path) {
                Ok(_) => Response::Status("Ok".to_string()),
                Err(e) => Response::Error(e.to_string()),
            }
        }
        Request::AddAlias { nickname, path } => {
            match db.add_alias(&nickname, &PathBuf::from(path)) {
                Ok(_) => Response::Status("Alias added".to_string()),
                Err(e) => Response::Error(e.to_string()),
            }
        }
        Request::RecordSelection { query, path } => {
            match db.record_selection(&query, &PathBuf::from(path)) {
                Ok(_) => Response::Status("Selection recorded".to_string()),
                Err(e) => Response::Error(e.to_string()),
            }
        }
        Request::ListAliases => match db.list_aliases() {
            Ok(aliases) => Response::AliasList(aliases),
            Err(e) => Response::Error(e.to_string()),
        },
        Request::RemoveAlias(nickname) => match db.remove_alias(&nickname) {
            Ok(_) => Response::Status("Alias removed".to_string()),
            Err(e) => Response::Error(e.to_string()),
        },
        Request::ClearAliases => match db.clear_aliases() {
            Ok(_) => Response::Status("All aliases cleared".to_string()),
            Err(e) => Response::Error(e.to_string()),
        },
        Request::ClearSelectionMemory => match db.clear_selection_memory() {
            Ok(_) => Response::Status("Smart learning memory cleared".to_string()),
            Err(e) => Response::Error(e.to_string()),
        },
        Request::Reindex => match index_all(&db) {
            Ok(_) => Response::Status("Reindex complete".to_string()),
            Err(e) => Response::Error(e.to_string()),
        },
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
        use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
        use std::sync::mpsc::channel;

        let (tx, rx) = channel();
        let mut watcher = match RecommendedWatcher::new(tx, Config::default()) {
            Ok(w) => w,
            Err(e) => {
                error!("Failed to create watcher: {}", e);
                return;
            }
        };

        for root in index_roots() {
            info!("Starting live filesystem watcher on {:?}", root);
            if let Err(e) = watcher.watch(&root, RecursiveMode::Recursive) {
                error!("Failed to watch {:?}: {}", root, e);
            }
        }

        for res in rx {
            match res {
                Ok(event) => match event.kind {
                    notify::EventKind::Create(_) => {
                        for path in event.paths {
                            if path.is_dir() {
                                let _ = db.add_directory(&path);
                            }
                        }
                    }
                    _ => {}
                },
                Err(e) => error!("watch error: {:?}", e),
            }
        }
    });
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    info!("Starting xerxesd...");

    let db_path = get_db_path()?;
    let db = Arc::new(XerxesDb::new(&db_path)?);
    info!("Opened DB at {:?}", db_path);

    {
        let db_clone = Arc::clone(&db);
        thread::spawn(move || {
            info!("Pre-warming directory cache...");
            if let Err(e) = db_clone.get_all_directories() {
                error!("Failed to pre-warm cache: {}", e);
            } else {
                info!("Directory cache warmed.");
            }
        });
    }

    if db.is_empty()? {
        info!("Database is empty, starting initial reindex...");
        let db_clone = Arc::clone(&db);
        thread::spawn(move || {
            if let Err(e) = index_all(&db_clone) {
                error!("Initial reindex failed: {}", e);
            } else {
                info!("Initial reindex complete.");
                let _ = db_clone.get_all_directories();
            }
        });
    }

    start_watcher(Arc::clone(&db));

    let socket_path = get_socket_path()?;
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }

    let listener = UnixListener::bind(&socket_path)?;
    info!("Listening on {:?}", socket_path);

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
