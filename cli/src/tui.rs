use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Terminal,
};
use std::fs::File;
use std::io::{ErrorKind, Write};
use std::process::{Command, Stdio};

pub fn select_path(paths: &[String]) -> Result<Option<String>> {
    // Fail fast without a terminal instead of leaving fzf waiting forever.
    File::open("/dev/tty").context("No terminal available to choose between matches")?;
    match select_with_fzf(paths) {
        Err(e)
            if e.downcast_ref::<std::io::Error>().map(|e| e.kind())
                == Some(ErrorKind::NotFound) =>
        {
            select_with_tui(paths)
        }
        res => res,
    }
}

// fzf draws its UI on /dev/tty itself, so only its stdin/stdout need piping.
fn select_with_fzf(paths: &[String]) -> Result<Option<String>> {
    let mut child = Command::new("fzf")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    {
        let stdin = child.stdin.as_mut().context("Failed to get stdin")?;
        for path in paths {
            writeln!(stdin, "{}", path)?;
        }
    }

    let output = child.wait_with_output().context("Failed to wait for fzf")?;
    if !output.status.success() {
        return Ok(None);
    }

    let res = String::from_utf8(output.stdout)
        .context("Invalid UTF-8 from fzf")?
        .trim()
        .to_string();
    Ok(if res.is_empty() { None } else { Some(res) })
}

fn filter(paths: &[String], query: &str) -> Vec<String> {
    let query = query.to_lowercase();
    paths
        .iter()
        .filter(|p| p.to_lowercase().contains(&query))
        .cloned()
        .collect()
}

// stdout is captured by the shell's $(...), so draw straight onto /dev/tty.
fn select_with_tui(paths: &[String]) -> Result<Option<String>> {
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("Failed to open /dev/tty for TUI")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(tty))?;

    enable_raw_mode()?;
    let result = execute!(terminal.backend_mut(), EnterAlternateScreen)
        .map_err(Into::into)
        .and_then(|_| run_tui(&mut terminal, paths));
    // Always restore the terminal, even if drawing failed.
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    result
}

fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<File>>,
    paths: &[String],
) -> Result<Option<String>> {
    let mut list_state = ListState::default();
    list_state.select(Some(0));

    let mut query = String::new();
    let mut filtered_paths: Vec<String> = paths.to_vec();
    let mut selection = None;

    loop {
        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
                .split(f.size());

            let input_text = format!("> {}", query);
            let input = Paragraph::new(input_text).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Filter (Type to refine) "),
            );
            f.render_widget(input, chunks[0]);

            let items: Vec<ListItem> = filtered_paths
                .iter()
                .map(|p| ListItem::new(p.as_str()))
                .collect();

            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(" Matches "))
                .highlight_style(
                    Style::default()
                        .bg(Color::Blue)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol(">> ");

            f.render_stateful_widget(list, chunks[1], &mut list_state);
        })?;

        if let Event::Key(key) = event::read().context("Failed to read TTY event")? {
            match key.code {
                KeyCode::Esc => break,
                KeyCode::Enter => {
                    if let Some(i) = list_state.selected() {
                        if i < filtered_paths.len() {
                            selection = Some(filtered_paths[i].clone());
                        }
                    }
                    break;
                }
                KeyCode::Down => {
                    let i = match list_state.selected() {
                        Some(i) => {
                            if i >= filtered_paths.len().saturating_sub(1) {
                                0
                            } else {
                                i + 1
                            }
                        }
                        None => 0,
                    };
                    list_state.select(Some(i));
                }
                KeyCode::Up => {
                    let i = match list_state.selected() {
                        Some(i) => {
                            if i == 0 {
                                filtered_paths.len().saturating_sub(1)
                            } else {
                                i - 1
                            }
                        }
                        None => 0,
                    };
                    list_state.select(Some(i));
                }
                KeyCode::Backspace => {
                    if query.pop().is_some() {
                        filtered_paths = filter(paths, &query);
                        list_state.select(Some(0));
                    }
                }
                KeyCode::Char(c) => {
                    query.push(c);
                    filtered_paths = filter(paths, &query);
                    list_state.select(Some(0));
                }
                _ => {}
            }
        }
    }

    Ok(selection)
}
