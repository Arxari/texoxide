use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use directories::ProjectDirs;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};
use rusqlite::{params, Connection};
use std::{
    env,
    io::{self, stdout},
    process::Command,
};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[arg(value_name = "QUERY")]
    query: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Remove {
        #[arg(value_name = "FILE_PATH")]
        file_path: String,
    },
}

struct TermUI {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TermUI {
    fn new() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        Ok(Self {
            terminal: Terminal::new(backend)?,
        })
    }

    fn restore(&mut self) -> Result<()> {
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        Ok(())
    }

    fn show_search_results(&mut self, items: &[String], title: &str) -> Result<Option<usize>> {
        let mut menu = Menu::new(items, title);
        loop {
            self.terminal.draw(|f| ui(f, &mut menu))?;

            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                match key.code {
                    KeyCode::Up => menu.previous(),
                    KeyCode::Down => menu.next(),
                    KeyCode::Enter => return Ok(menu.state.selected()),
                    KeyCode::Esc => return Ok(None),
                    _ => {}
                }
            }
        }
    }
}

fn clean_path(path: &str) -> &str {
    path.strip_prefix("\\\\?\\").unwrap_or(path)
}

impl Drop for TermUI {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn ui(f: &mut Frame, menu: &mut Menu) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints(
            [
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(3),
            ]
            .as_ref(),
        )
        .split(f.area());

    // Title
    let title = Paragraph::new(menu.title.clone())
        .style(Style::default().add_modifier(Modifier::BOLD))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::BOTTOM));
    f.render_widget(title, chunks[0]);

    // List
    if !menu.items.is_empty() {
        let items: Vec<ListItem> = menu
            .items
            .iter()
            .map(|i| {
                let display = if cfg!(windows) {
                    clean_path(i)
                } else {
                    i.as_str()
                };
                ListItem::new(display).style(Style::default().fg(Color::White))
            })
            .collect();

        let list_widget = List::new(items)
            .block(Block::default().borders(Borders::NONE))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">> ");

        f.render_stateful_widget(list_widget, chunks[1], &mut menu.state);
    }

    // Controls
    let instructions = Line::from(vec![
        Span::styled("↑/↓", Style::default().fg(Color::Yellow)),
        Span::raw(" Navigate  "),
        Span::styled("Enter", Style::default().fg(Color::Green)),
        Span::raw(" Select  "),
        Span::styled("Esc", Style::default().fg(Color::Red)),
        Span::raw(" Exit"),
    ]);

    let footer = Paragraph::new(instructions)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::TOP));
    f.render_widget(footer, chunks[2]);
}

struct Texoxide {
    conn: Connection,
}

impl Texoxide {
    fn new() -> Result<Self> {
        let dirs = ProjectDirs::from("", "", "texoxide")
            .context("Could not determine project directories")?;
        let data_dir = dirs.data_dir();
        std::fs::create_dir_all(data_dir).context("Failed to create data directory")?;
        let db_path = data_dir.join("texoxide.db");

        let conn = Connection::open(db_path).context("Failed to open database")?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS files (
                path TEXT PRIMARY KEY,
                last_accessed DATETIME DEFAULT CURRENT_TIMESTAMP,
                frequency INTEGER DEFAULT 1
            )",
            [],
        )
        .context("Failed to create database schema")?;

        Ok(Self { conn })
    }

    fn add(&self, file_path: &str) -> Result<()> {
        let path = Utf8Path::new(file_path);
        if !path.as_std_path().exists() {
            anyhow::bail!("File {file_path} does not exist");
        }

        let canonical = path.as_std_path().canonicalize()?;
        let abs_path = Utf8PathBuf::from_path_buf(canonical)
            .map_err(|_| anyhow::anyhow!("Invalid file path encoding"))?
            .to_string();

        self.conn.execute(
            "INSERT INTO files (path, frequency) VALUES (?, 1)
             ON CONFLICT(path) DO UPDATE SET
                 frequency = frequency + 1,
                 last_accessed = CURRENT_TIMESTAMP",
            params![&abs_path],
        )?;
        Ok(())
    }

    fn remove_entry(&self, file_path: &str) -> Result<()> {
        let path = Utf8Path::new(file_path);
        let abs_path = path
            .as_std_path()
            .canonicalize()
            .ok()
            .and_then(|p| Utf8PathBuf::from_path_buf(p).ok())
            .map_or_else(|| file_path.to_string(), |p| p.to_string());

        let count = self
            .conn
            .execute("DELETE FROM files WHERE path = ?", params![abs_path])?;
        if count == 0 {
            anyhow::bail!("No entry found for {file_path}");
        }
        Ok(())
    }

    fn cleanup(&self) -> Result<()> {
        let mut stmt = self.conn.prepare("SELECT path FROM files")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut to_remove = Vec::new();
        for row in rows {
            let path: String = row?;
            if !Utf8Path::new(&path).as_std_path().exists() {
                to_remove.push(path);
            }
        }
        for path in to_remove {
            self.conn
                .execute("DELETE FROM files WHERE path = ?", params![path])?;
        }
        Ok(())
    }

    fn query(&self, search_term: &str) -> Result<Vec<String>> {
        let pattern = format!("%{search_term}%");
        let mut stmt = self.conn.prepare(
            "SELECT path
            FROM files
            WHERE path LIKE ? ESCAPE '\\'
            ORDER BY frequency DESC, last_accessed DESC
            LIMIT 20",
        )?;

        let mut rows = stmt.query(params![pattern])?;
        let mut results = Vec::new();
        while let Some(row) = rows.next()? {
            results.push(row.get(0)?);
        }
        Ok(results)
    }
}

fn open_file(file_path: &str) -> Result<()> {
    #[cfg(windows)]
    let editor = env::var("EDITOR").unwrap_or_else(|_| "notepad".to_string());
    #[cfg(not(windows))]
    let editor = env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());

    Command::new(editor)
        .arg(file_path)
        .status()
        .context("Failed to open $EDITOR")?;
    Ok(())
}

struct Menu<'a> {
    state: ListState,
    items: &'a [String],
    title: String,
}

impl<'a> Menu<'a> {
    fn new(items: &'a [String], title: &str) -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        Self {
            state,
            items,
            title: title.to_string(),
        }
    }

    fn next(&mut self) {
        let i = self
            .state
            .selected()
            .map_or(0, |i| if i >= self.items.len() - 1 { 0 } else { i + 1 });
        self.state.select(Some(i));
    }

    fn previous(&mut self) {
        let i = self
            .state
            .selected()
            .map_or(0, |i| if i == 0 { self.items.len() - 1 } else { i - 1 });
        self.state.select(Some(i));
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let texoxide = Texoxide::new()?;

    if let Some(Commands::Remove { file_path }) = cli.command {
        texoxide.remove_entry(&file_path)?;
        println!("Removed {file_path} from list");
        return Ok(());
    }

    let mut ui = TermUI::new()?;
    texoxide.cleanup()?;

    let search_term = cli.query.as_deref().unwrap_or("");
    let results = texoxide.query(search_term)?;

    if !results.is_empty() {
        let selection =
            ui.show_search_results(&results, &format!(" Matches for '{search_term}' "))?;
        if let Some(idx) = selection {
            let path = &results[idx];
            texoxide.add(path)?;
            open_file(path)?;
        }
    } else if !search_term.is_empty() && Utf8Path::new(search_term).as_std_path().exists() {
        texoxide.add(search_term)?;
        open_file(search_term)?;
    } else {
        eprintln!("No matches for '{search_term}'");
    }

    Ok(())
}
