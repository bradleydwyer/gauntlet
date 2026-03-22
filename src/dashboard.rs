//! TUI dashboard for gauntlet serve.
//!
//! Shows active and recent builds with real-time task status and log viewing.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use tokio::sync::Mutex;

use tasked::engine::Engine;
use tasked::types::{FlowId, FlowState, TaskState};

/// A build entry for the dashboard.
#[derive(Debug, Clone)]
pub struct BuildEntry {
    pub flow_id: String,
    pub repo: String,
    pub sha: String,
    pub state: FlowState,
    pub task_count: usize,
    pub tasks_succeeded: usize,
    pub tasks_failed: usize,
}

/// A webhook event log entry.
#[derive(Debug, Clone)]
pub struct WebhookEvent {
    pub time: chrono::DateTime<chrono::Utc>,
    pub repo: String,
    pub event_type: String,
    pub detail: String,
}

/// Shared dashboard state, updated by the build monitor.
pub struct DashboardState {
    pub active_builds: Vec<BuildEntry>,
    pub recent_builds: VecDeque<BuildEntry>,
    pub webhook_log: VecDeque<WebhookEvent>,
    pub engine: Arc<Engine>,
}

impl DashboardState {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            active_builds: Vec::new(),
            recent_builds: VecDeque::with_capacity(50),
            webhook_log: VecDeque::with_capacity(100),
            engine,
        }
    }

    pub fn log_webhook(&mut self, repo: String, event_type: String, detail: String) {
        self.webhook_log.push_front(WebhookEvent {
            time: chrono::Utc::now(),
            repo,
            event_type,
            detail,
        });
        if self.webhook_log.len() > 100 {
            self.webhook_log.pop_back();
        }
    }

    pub fn add_build(&mut self, entry: BuildEntry) {
        self.active_builds.push(entry);
    }

    pub fn complete_build(
        &mut self,
        flow_id: &str,
        state: FlowState,
        succeeded: usize,
        failed: usize,
    ) {
        if let Some(pos) = self.active_builds.iter().position(|b| b.flow_id == flow_id) {
            let mut entry = self.active_builds.remove(pos);
            entry.state = state;
            entry.tasks_succeeded = succeeded;
            entry.tasks_failed = failed;
            self.recent_builds.push_front(entry);
            if self.recent_builds.len() > 50 {
                self.recent_builds.pop_back();
            }
        }
    }
}

#[derive(PartialEq)]
enum View {
    Builds,
    Tasks,
    Log,
}

struct App {
    view: View,
    build_list_state: ListState,
    task_list_state: ListState,
    selected_flow_id: Option<String>,
    log_scroll: u16,
}

impl App {
    fn new() -> Self {
        let mut build_list_state = ListState::default();
        build_list_state.select(Some(0));
        Self {
            view: View::Builds,
            build_list_state,
            task_list_state: ListState::default(),
            selected_flow_id: None,
            log_scroll: 0,
        }
    }
}

/// Run the TUI dashboard. This takes over the terminal.
pub async fn run_dashboard(dashboard: Arc<Mutex<DashboardState>>) -> std::io::Result<()> {
    enable_raw_mode()?;
    std::io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();

    loop {
        // Draw + ensure selection is valid.
        let dash = dashboard.lock().await;
        let total_builds = dash.active_builds.len() + dash.recent_builds.len();
        if total_builds > 0 && app.build_list_state.selected().is_none() {
            app.build_list_state.select(Some(0));
        }
        terminal.draw(|f| draw_ui(f, &dash, &mut app))?;
        drop(dash);

        // Handle input (non-blocking, 100ms timeout).
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Esc => match app.view {
                    View::Log => {
                        app.view = View::Tasks;
                        app.log_scroll = 0;
                    }
                    View::Tasks => {
                        app.view = View::Builds;
                        app.selected_flow_id = None;
                    }
                    View::Builds => break,
                },
                KeyCode::Enter => {
                    let dash = dashboard.lock().await;
                    match app.view {
                        View::Builds => {
                            let all_builds: Vec<&BuildEntry> = dash
                                .active_builds
                                .iter()
                                .chain(dash.recent_builds.iter())
                                .collect();
                            if !all_builds.is_empty() {
                                // Ensure selection is valid.
                                let idx = app.build_list_state.selected().unwrap_or(0);
                                let idx = idx.min(all_builds.len() - 1);
                                if let Some(build) = all_builds.get(idx) {
                                    app.selected_flow_id = Some(build.flow_id.clone());
                                    app.view = View::Tasks;
                                    app.task_list_state.select(Some(0));
                                }
                            }
                        }
                        View::Tasks => {
                            app.view = View::Log;
                            app.log_scroll = 0;
                        }
                        View::Log => {}
                    }
                }
                KeyCode::Up => match app.view {
                    View::Builds => {
                        let i = app.build_list_state.selected().unwrap_or(0);
                        if i > 0 {
                            app.build_list_state.select(Some(i - 1));
                        }
                    }
                    View::Tasks => {
                        let i = app.task_list_state.selected().unwrap_or(0);
                        if i > 0 {
                            app.task_list_state.select(Some(i - 1));
                        }
                    }
                    View::Log => {
                        app.log_scroll = app.log_scroll.saturating_sub(3);
                    }
                },
                KeyCode::Down => match app.view {
                    View::Builds => {
                        let dash2 = dashboard.lock().await;
                        let total = dash2.active_builds.len() + dash2.recent_builds.len();
                        let i = app.build_list_state.selected().unwrap_or(0);
                        if i + 1 < total {
                            app.build_list_state.select(Some(i + 1));
                        }
                    }
                    View::Tasks => {
                        let i = app.task_list_state.selected().unwrap_or(0);
                        app.task_list_state.select(Some(i + 1));
                    }
                    View::Log => {
                        app.log_scroll = app.log_scroll.saturating_add(3);
                    }
                },
                _ => {}
            }
        }
    }

    disable_raw_mode()?;
    std::io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

fn draw_ui(f: &mut ratatui::Frame, dash: &DashboardState, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // Main content
            Constraint::Length(1), // Footer
        ])
        .split(f.area());

    // Header.
    let active = dash.active_builds.len();
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " gauntlet ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {active} active build{}",
            if active == 1 { "" } else { "s" }
        )),
    ]))
    .block(Block::default().borders(Borders::BOTTOM));
    f.render_widget(header, chunks[0]);

    // Main content.
    match app.view {
        View::Builds => draw_builds(f, dash, app, chunks[1]),
        View::Tasks => draw_tasks(f, dash, app, chunks[1]),
        View::Log => draw_log(f, dash, app, chunks[1]),
    }

    // Footer.
    let help = match app.view {
        View::Builds => " ↑↓ navigate  ⏎ view tasks  q quit",
        View::Tasks => " ↑↓ navigate  ⏎ view log  esc back  q quit",
        View::Log => " ↑↓ scroll  esc back  q quit",
    };
    let footer = Paragraph::new(Span::styled(help, Style::default().fg(Color::DarkGray)));
    f.render_widget(footer, chunks[2]);
}

fn draw_builds(f: &mut ratatui::Frame, dash: &DashboardState, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(8)])
        .split(area);

    // Top: builds list.
    let items: Vec<ListItem> = dash
        .active_builds
        .iter()
        .chain(dash.recent_builds.iter())
        .map(|b| {
            let (icon, color) = match b.state {
                FlowState::Running => ("⟳", Color::Yellow),
                FlowState::Succeeded => ("✓", Color::Green),
                FlowState::Failed => ("✗", Color::Red),
                FlowState::Cancelled => ("⊘", Color::DarkGray),
            };
            let sha = &b.sha[..7.min(b.sha.len())];
            let progress = format!("{}/{}", b.tasks_succeeded, b.task_count);
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {icon} "), Style::default().fg(color)),
                Span::styled(&b.repo, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!("  {sha}  [{progress}]")),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().title(" Builds ").borders(Borders::ALL))
        .highlight_style(Style::default().bg(Color::DarkGray));
    f.render_stateful_widget(list, chunks[0], &mut app.build_list_state);

    // Bottom: webhook log.
    let webhook_items: Vec<ListItem> = dash
        .webhook_log
        .iter()
        .map(|w| {
            let time = w.time.format("%H:%M:%S").to_string();
            let repo_short = w.repo.split('/').next_back().unwrap_or(&w.repo);
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {time} "), Style::default().fg(Color::DarkGray)),
                Span::styled(repo_short, Style::default().fg(Color::Cyan)),
                Span::raw(format!(" {} {}", w.event_type, w.detail)),
            ]))
        })
        .collect();

    let webhook_list =
        List::new(webhook_items).block(Block::default().title(" Webhooks ").borders(Borders::ALL));
    f.render_widget(webhook_list, chunks[1]);
}

fn draw_tasks(f: &mut ratatui::Frame, dash: &DashboardState, app: &mut App, area: Rect) {
    let flow_id = match &app.selected_flow_id {
        Some(id) => id.clone(),
        None => return,
    };

    // Get tasks from engine synchronously (we're in the render loop).
    // We can't await here, so we use a cached approach — check active builds for info.
    let build = dash
        .active_builds
        .iter()
        .chain(dash.recent_builds.iter())
        .find(|b| b.flow_id == flow_id);

    let title = match build {
        Some(b) => format!(" {} @ {} ", b.repo, &b.sha[..7.min(b.sha.len())]),
        None => format!(" {flow_id} "),
    };

    // We need tasks — try to get them from the engine.
    // Since we can't await in draw, we'll use tokio::task::block_in_place.
    let tasks = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            dash.engine
                .get_flow_tasks(&FlowId(flow_id.clone()))
                .await
                .unwrap_or_default()
        })
    });

    let items: Vec<ListItem> = tasks
        .iter()
        .map(|t| {
            let (icon, color) = match t.state {
                TaskState::Succeeded => ("✓", Color::Green),
                TaskState::Failed => ("✗", Color::Red),
                TaskState::Running => ("⟳", Color::Yellow),
                TaskState::Cancelled => ("⊘", Color::DarkGray),
                TaskState::Pending | TaskState::Ready => ("○", Color::DarkGray),
                TaskState::Delayed => ("↻", Color::Yellow),
            };
            let duration = match (t.started_at, t.completed_at) {
                (Some(start), Some(end)) => format!("{}s", (end - start).num_seconds()),
                (Some(start), None) => {
                    let elapsed = (chrono::Utc::now() - start).num_seconds();
                    format!("{elapsed}s...")
                }
                _ => "-".to_string(),
            };
            let error_suffix = if let Some(ref err) = t.error {
                format!("  {}", &err[..60.min(err.len())])
            } else {
                String::new()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {icon} "), Style::default().fg(color)),
                Span::styled(&t.id.0, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!("  ({duration}){error_suffix}")),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL))
        .highlight_style(Style::default().bg(Color::DarkGray));
    f.render_stateful_widget(list, area, &mut app.task_list_state);
}

fn draw_log(f: &mut ratatui::Frame, dash: &DashboardState, app: &mut App, area: Rect) {
    let flow_id = match &app.selected_flow_id {
        Some(id) => id.clone(),
        None => return,
    };

    let tasks = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            dash.engine
                .get_flow_tasks(&FlowId(flow_id.clone()))
                .await
                .unwrap_or_default()
        })
    });

    let task_idx = app.task_list_state.selected().unwrap_or(0);
    let task = match tasks.get(task_idx) {
        Some(t) => t,
        None => return,
    };

    let mut lines = Vec::new();

    if let Some(ref error) = task.error {
        lines.push(Line::from(Span::styled(
            format!("ERROR: {error}"),
            Style::default().fg(Color::Red),
        )));
        lines.push(Line::from(""));
    }

    if let Some(ref output) = task.output {
        if let Some(stdout) = output.get("stdout").and_then(|v| v.as_str()) {
            for line in stdout.lines() {
                lines.push(Line::from(line.to_string()));
            }
        }
        if let Some(stderr) = output.get("stderr").and_then(|v| v.as_str())
            && !stderr.is_empty()
        {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "--- stderr ---",
                Style::default().fg(Color::Yellow),
            )));
            for line in stderr.lines() {
                lines.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    } else {
        lines.push(Line::from(Span::styled(
            format!("({}) waiting for output...", task.state),
            Style::default().fg(Color::DarkGray),
        )));
    }

    let title = format!(" {} ", task.id.0);
    let paragraph = Paragraph::new(lines)
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false })
        .scroll((app.log_scroll, 0));
    f.render_widget(paragraph, area);
}
