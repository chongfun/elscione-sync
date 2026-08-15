use anyhow::Result;
use bytesize::ByteSize;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};
use std::io;
use tracing::{info, warn};

use crate::config::Config;
use crate::crawler::{parser, session::ElscioneSession};
use crate::db::{models, Db};

/// A folder node in the checkbox tree.
#[derive(Debug, Clone)]
pub struct FolderNode {
    /// Display path relative to server root, e.g. "Manga".
    pub path: String,
    /// Depth (0 = top-level).
    pub depth: usize,
    /// Whether this folder is selected for download.
    pub selected: bool,
    /// Aggregate size of all files under this folder (if known).
    pub size_bytes: Option<i64>,
}

struct App {
    nodes: Vec<FolderNode>,
    state: ListState,
}

impl App {
    fn new(nodes: Vec<FolderNode>) -> Self {
        let mut state = ListState::default();
        if !nodes.is_empty() {
            state.select(Some(0));
        }
        Self { nodes, state }
    }

    fn selected_index(&self) -> Option<usize> {
        self.state.selected()
    }

    fn move_up(&mut self) {
        if self.nodes.is_empty() {
            return;
        }
        let i = self.state.selected().unwrap_or(0);
        self.state
            .select(Some(if i == 0 { self.nodes.len() - 1 } else { i - 1 }));
    }

    fn move_down(&mut self) {
        if self.nodes.is_empty() {
            return;
        }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some((i + 1) % self.nodes.len()));
    }

    fn toggle_selected(&mut self) {
        if let Some(i) = self.selected_index() {
            self.nodes[i].selected = !self.nodes[i].selected;
        }
    }

    fn select_all(&mut self, value: bool) {
        for n in &mut self.nodes {
            n.selected = value;
        }
    }

    /// Total bytes of all selected folders.
    fn total_selected_bytes(&self) -> i64 {
        self.nodes
            .iter()
            .filter(|n| n.selected)
            .filter_map(|n| n.size_bytes)
            .sum()
    }

    fn selected_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.selected).count()
    }
}

/// Fetch top-level folders from the server and build FolderNode list.
/// Uses the h5ai JSON API directly (the server is a JS-rendered h5ai instance).
async fn fetch_folders(config: &Config) -> Result<Vec<FolderNode>> {
    let session = ElscioneSession::new(
        &config.server.base_url,
        &config.server.user_agent,
        config.server.cookie.as_deref(),
    )?;
    info!("Fetching folder list from {}", config.server.base_url);

    // Try h5ai JSON API — this server is confirmed to run h5ai.
    match crate::crawler::try_h5ai(
        &session,
        &config.server.base_url,
        &config.server.base_url,
    )
    .await
    {
        Ok(entries) => {
            if !entries.is_empty() {
                return Ok(build_nodes(entries));
            }
            eprintln!("h5ai API returned an empty response for the root directory.");
        }
        Err(error) => {
            warn!("h5ai API request failed: {error}. Try running with --verbose for details.");
        }
    }

    Ok(vec![])
}

/// Convert a raw DirEntry list (directories only) into FolderNode values.
fn build_nodes(entries: Vec<parser::DirEntry>) -> Vec<FolderNode> {
    entries
        .into_iter()
        .filter(|e| e.is_dir)
        .map(|e| FolderNode {
            path: e.name.clone(),
            depth: 0,
            selected: false,
            size_bytes: e.size_bytes,
        })
        .collect()
}

fn render_ui(f: &mut Frame, app: &App) {
    let area = f.area();

    // Layout: title | list | status bar
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // title
            Constraint::Min(0),    // list
            Constraint::Length(3), // status bar
        ])
        .split(area);

    // ── Title ──
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            "elscione-sync",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  ·  Select folders to mirror"),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    // ── Folder List ──
    let items: Vec<ListItem> = app
        .nodes
        .iter()
        .map(|node| {
            let checkbox = if node.selected { "[ ✓ ]" } else { "[   ]" };
            let indent = "  ".repeat(node.depth);
            let size_str = node
                .size_bytes
                .map(|b| format!(" ({})", ByteSize(b as u64)))
                .unwrap_or_default();

            let style = if node.selected {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::White)
            };

            let line = Line::from(vec![
                Span::raw(indent),
                Span::styled(checkbox, style),
                Span::raw("  "),
                Span::styled(node.path.clone(), style.add_modifier(Modifier::BOLD)),
                Span::styled(size_str, Style::default().fg(Color::DarkGray)),
            ]);

            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Folders ")
                .title_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, chunks[1], &mut app.state.clone());

    // ── Status Bar ──
    let total_bytes = app.total_selected_bytes();
    let selected_count = app.selected_count();
    let total_size_str = if total_bytes > 0 {
        format!("{}", ByteSize(total_bytes as u64))
    } else {
        "—".to_owned()
    };

    let help_line = Line::from(vec![
        Span::styled(
            format!(
                " Selected: {} folder{}  |  Total: {}  ",
                selected_count,
                if selected_count == 1 { "" } else { "s" },
                total_size_str
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  [Space] toggle  [a] all  [n] none  [↑↓/jk] navigate  [Enter] confirm  [q] quit",
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let status = Paragraph::new(help_line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(status, chunks[2]);
}

/// Drive the selector until the user confirms (`Some(nodes)`) or cancels (`None`).
fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<Option<Vec<FolderNode>>> {
    loop {
        terminal.draw(|f| render_ui(f, app))?;

        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        return Ok(None);
                    }
                    KeyCode::Enter => {
                        return Ok(Some(app.nodes.clone()));
                    }
                    KeyCode::Char(' ') => {
                        app.toggle_selected();
                    }
                    KeyCode::Char('a') => {
                        app.select_all(true);
                    }
                    KeyCode::Char('n') => {
                        app.select_all(false);
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        app.move_up();
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        app.move_down();
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Run the interactive TUI folder selector.
pub async fn run(config: &Config, db: &Db) -> Result<()> {
    // Fetch top-level folders — print progress before entering raw mode.
    eprintln!("Fetching folder list from {} …", config.server.base_url);
    let nodes = fetch_folders(config).await?;
    if nodes.is_empty() {
        eprintln!("Warning: no folders found on the server. The site may require JavaScript.");
        eprintln!("Try running with --verbose to see the raw server response.");
    } else {
        eprintln!("Found {} top-level folder(s).", nodes.len());
    }

    // Load any existing selections from DB.
    let existing_selections: Vec<String> =
        crate::db::run_blocking(db, |conn| Ok(models::load_selected_folders(conn)?))
            .await?
            .into_iter()
            .filter(|f| f.enabled)
            .map(|f| f.path)
            .collect();

    let nodes: Vec<FolderNode> = nodes
        .into_iter()
        .map(|mut n| {
            n.selected = existing_selections.iter().any(|s| s == &n.path);
            n
        })
        .collect();

    let mut app = App::new(nodes);

    // Enter TUI.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run the event loop in a helper so any error still reaches the terminal
    // cleanup below instead of leaving raw mode / the alternate screen active.
    let result = event_loop(&mut terminal, &mut app);

    // Restore terminal regardless of outcome.
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    match result? {
        None => {
            println!("Folder selection cancelled.");
        }
        Some(nodes) => {
            let folders: Vec<models::SelectedFolder> = nodes
                .iter()
                .map(|n| models::SelectedFolder {
                    path: n.path.clone(),
                    enabled: n.selected,
                    size_bytes: n.size_bytes,
                })
                .collect();

            let selected_count = folders.iter().filter(|f| f.enabled).count();

            crate::db::run_blocking(db, move |conn| {
                Ok(models::save_selected_folders(conn, &folders)?)
            })
            .await?;

            println!(
                "Saved {} folder selection{}.",
                selected_count,
                if selected_count == 1 { "" } else { "s" }
            );
        }
    }

    Ok(())
}
