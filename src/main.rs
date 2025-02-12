use anyhow::{Context, Result};
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
    path::Path,
    process::Command,
};

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
            .map(|i| ListItem::new(i.as_str()).style(Style::default().fg(Color::White)))
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
        let path = Path::new(file_path);
        if !path.exists() {
            anyhow::bail!("File {file_path} does not exist");
        }

        let abs_path = path
            .canonicalize()?
            .to_str()
            .context("Invalid file path encoding")?
            .to_string();

        let mut stmt = self.conn.prepare(
            "UPDATE files
            SET last_accessed = CURRENT_TIMESTAMP,
                frequency = frequency + 1
            WHERE path = ?",
        )?;

        if stmt.execute(params![&abs_path])? == 0 {
            self.conn.execute(
                "INSERT INTO files (path, frequency)
                VALUES (?, 1)",
                params![&abs_path],
            )?;
        }

        Ok(())
    }

    fn remove_entry(&self, file_path: &str) -> Result<()> {
        let path = Path::new(file_path);

        let abs_path = path
            .canonicalize()
            .unwrap_or_else(|_| file_path.into())
            .to_string_lossy()
            .into_owned();

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
            if !Path::new(&path).exists() {
                to_remove.push(path);
            }
        }
        for path in to_remove {
            self.conn
                .execute("DELETE FROM files WHERE path = ?", params![path])?;
        }
        Ok(())
    }

    #[allow(clippy::unused_self)]
    fn open_file(&self, file_path: &str) -> Result<()> {
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
    let args: Vec<String> = env::args().collect();
    let mut ui = TermUI::new()?;
    let texoxide = Texoxide::new()?;

    if args.len() > 1 && args[1] == "remove" {
        if args.len() > 2 {
            let file_to_remove = &args[2];
            texoxide.remove_entry(file_to_remove)?;
            println!("Removed {file_to_remove} from list");
        } else {
            eprintln!("Usage: {} remove <file_path>", args[0]);
        }
        return Ok(());
    }

    texoxide.cleanup()?;

    let search_term = args.get(1).map_or("", String::as_str);
    let results = texoxide.query(search_term)?;

    if !results.is_empty() {
        let selection =
            ui.show_search_results(&results, &format!(" Matches for '{search_term}' "))?;
        if let Some(idx) = selection {
            let path = &results[idx];
            texoxide.add(path)?;
            texoxide.open_file(path)?;
        }
    } else if Path::new(search_term).exists() {
        texoxide.add(search_term)?;
        texoxide.open_file(search_term)?;
    } else {
        eprintln!("No matches for '{search_term}'");
    }

    Ok(())
}
