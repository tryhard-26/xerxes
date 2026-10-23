use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use xerxes_core::ipc::{Request, Response};

mod tui;

#[derive(Parser)]
#[command(
    name = "xerxes",
    about = "High-performance filesystem navigation engine",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(
        trailing_var_arg = true,
        help = "The query string to search for (if no subcommand is provided)"
    )]
    query: Vec<String>,
}

#[derive(Subcommand)]
enum Commands {
    Reindex,
    Stats,
    Doctor,
    Init {
        shell: String,
    },
    Setup {
        #[arg(long)]
        path: Option<PathBuf>,
    },
    Alias {
        path: Option<PathBuf>,
        nickname: Option<String>,
        #[arg(long)]
        list: bool,
        #[arg(long)]
        remove: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    ClearMemory,
}

fn get_socket_path() -> Result<PathBuf> {
    let proj_dirs = ProjectDirs::from("com", "xerxes", "xerxes")
        .context("Could not find project directories")?;
    let run_dir = proj_dirs
        .runtime_dir()
        .unwrap_or_else(|| proj_dirs.data_dir());
    Ok(run_dir.join("xerxesd.sock"))
}

fn connect() -> Result<UnixStream> {
    let socket_path = get_socket_path()?;
    if let Ok(stream) = UnixStream::connect(&socket_path) {
        return Ok(stream);
    }

    let daemon_exe = std::env::current_exe()?.with_file_name("_xerxesd");
    std::process::Command::new(&daemon_exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("Failed to spawn _xerxesd. Is it installed in the same directory?")?;

    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if let Ok(stream) = UnixStream::connect(&socket_path) {
            return Ok(stream);
        }
    }
    anyhow::bail!("Timed out waiting for _xerxesd to start.")
}

fn send_request(req: Request) -> Result<Response> {
    let mut stream = connect()?;

    // Reindex walks the whole tree; let it take as long as it needs.
    if !matches!(req, Request::Reindex) {
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    }
    stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;

    stream.write_all(&bincode::serialize(&req)?)?;
    stream.shutdown(Shutdown::Write)?;

    let mut response_bytes = Vec::new();
    stream.read_to_end(&mut response_bytes)?;
    Ok(bincode::deserialize(&response_bytes)?)
}

fn print_response(response: Response) {
    match response {
        Response::Status(s) => println!("{}", s),
        Response::AliasList(aliases) if aliases.is_empty() => println!("No aliases"),
        Response::AliasList(aliases) => {
            for (n, p) in aliases {
                println!("{}: {}", n, p);
            }
        }
        Response::Error(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        r => println!("{:?}", r),
    }
}

// The wrapper only cds when _xerxes printed a directory (or "-"); anything else,
// like subcommand output, is shown instead of being fed to cd.
fn print_shell_init(shell: &str) {
    match shell {
        "zsh" | "bash" => {
            print!(
                r#"
x() {{
    local dir
    dir="$(_xerxes "$@")" || return
    if [ "$dir" = "-" ] || [ -d "$dir" ]; then
        cd "$dir"
    elif [ -n "$dir" ]; then
        printf '%s\n' "$dir"
    fi
}}
"#
            );
            if shell == "zsh" {
                println!("if command -v compdef >/dev/null 2>&1; then\n    compdef _cd x\nfi");
            } else {
                println!("complete -d x");
            }
        }
        "fish" => {
            println!(
                r#"
function x
    set -l dir (_xerxes $argv | string collect); or return
    if test "$dir" = "-"; or test -d "$dir"
        cd "$dir"
    else if test -n "$dir"
        printf '%s\n' "$dir"
    end
end
complete -c x -a "(__fish_complete_directories)""#
            );
        }
        _ => {
            eprintln!("Unsupported shell. Please use zsh, bash, or fish.");
        }
    }
}

fn run_setup(custom_path: Option<PathBuf>) -> Result<()> {
    use directories::UserDirs;
    use std::fs::OpenOptions;

    let user_dirs = UserDirs::new().context("Could not determine user directories")?;
    let home = user_dirs.home_dir();
    let exe = std::env::current_exe()?;

    let shell_env = std::env::var("SHELL").unwrap_or_default();
    let Some((shell, default_rc)) = [
        ("zsh", ".zshrc"),
        ("bash", ".bashrc"),
        ("fish", ".config/fish/config.fish"),
    ]
    .into_iter()
    .find(|(name, _)| shell_env.contains(name)) else {
        println!("Unsupported shell. Run `_xerxes init <zsh|bash|fish>` and add it to your config manually.");
        return Ok(());
    };

    let init_line = if shell == "fish" {
        format!("\n{} init fish | source\n", exe.display())
    } else {
        format!("\neval \"$({} init {})\"\n", exe.display(), shell)
    };

    let path = match custom_path {
        Some(p) => match p.strip_prefix("~") {
            Ok(rest) => home.join(rest),
            Err(_) => p,
        },
        None => home.join(default_rc),
    };

    if !path.exists() {
        println!("Could not find shell config file at {:?}", path);
    } else if std::fs::read_to_string(&path)?.contains("_xerxes init") {
        println!("Xerxes is already configured in {:?}", path);
    } else {
        OpenOptions::new()
            .append(true)
            .open(&path)?
            .write_all(init_line.as_bytes())?;
        println!("Added _xerxes init to {:?}", path);
        println!("Please restart your shell or run: source {:?}", path);
    }

    Ok(())
}

fn handle_query(query_parts: &[String]) -> Result<()> {
    let query_str = query_parts.join(" ");
    if query_str.is_empty() {
        return Ok(());
    }

    if query_str == ".." || query_str == "-" || query_str == "/" {
        println!("{}", query_str);
        return Ok(());
    }
    if query_str == "~" {
        let home = directories::UserDirs::new().context("Could not determine home directory")?;
        println!("{}", home.home_dir().display());
        return Ok(());
    }

    let path = std::path::Path::new(&query_str);
    if path.is_dir() {
        if let Ok(abs_path) = std::fs::canonicalize(path) {
            let p = abs_path.to_string_lossy().into_owned();
            let _ = send_request(Request::Select(p.clone()));
            println!("{}", p);
            return Ok(());
        }
    }

    match send_request(Request::Query(query_str.clone()))? {
        Response::QueryResults(paths) => {
            let (selected, picked) = match paths.as_slice() {
                [] => {
                    eprintln!("xerxes: no match for '{}'", query_str);
                    std::process::exit(1);
                }
                [(only, _)] => (Some(only.clone()), false),
                [(top, a), (_, b), ..] if a - b > 50.0 => (Some(top.clone()), false),
                _ => {
                    let path_strings: Vec<String> = paths.into_iter().map(|(p, _)| p).collect();
                    (tui::select_path(&path_strings)?, true)
                }
            };

            if let Some(p) = selected {
                let _ = send_request(Request::Select(p.clone()));
                // Only remember explicit picks; auto-picks would freeze today's ranking forever.
                if picked {
                    let _ = send_request(Request::RecordSelection {
                        query: query_str,
                        path: p.clone(),
                    });
                }
                println!("{}", p);
            }
        }
        Response::Error(e) => {
            eprintln!("Error from daemon: {}", e);
            std::process::exit(1);
        }
        _ => {
            eprintln!("Unexpected response from daemon.");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(cmd) = cli.command {
        match cmd {
            Commands::Reindex => print_response(send_request(Request::Reindex)?),
            Commands::Stats => print_response(send_request(Request::Stats)?),
            Commands::Doctor => print_response(send_request(Request::Doctor)?),
            Commands::Init { shell } => {
                print_shell_init(&shell);
            }
            Commands::Setup { path } => {
                run_setup(path)?;
            }
            Commands::Alias {
                path,
                nickname,
                list,
                remove,
                clear,
            } => {
                let response = if list {
                    send_request(Request::ListAliases)?
                } else if let Some(n) = remove {
                    send_request(Request::RemoveAlias(n))?
                } else if clear {
                    send_request(Request::ClearAliases)?
                } else if let (Some(p), Some(n)) = (path, nickname) {
                    let abs_path = std::fs::canonicalize(&p)
                        .with_context(|| format!("Could not resolve path {:?}", p))?;
                    if !abs_path.is_dir() {
                        anyhow::bail!("{:?} is not a directory", abs_path);
                    }
                    send_request(Request::AddAlias {
                        nickname: n,
                        path: abs_path.to_string_lossy().into_owned(),
                    })?
                } else {
                    eprintln!("Usage: x alias <path> <nickname> OR x alias --list OR x alias --remove <nickname> OR x alias --clear");
                    std::process::exit(2);
                };
                print_response(response);
            }
            Commands::ClearMemory => print_response(send_request(Request::ClearSelectionMemory)?),
        }
    } else {
        handle_query(&cli.query)?;
    }

    Ok(())
}
