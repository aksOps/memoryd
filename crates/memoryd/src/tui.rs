//! `memoryd tui`: read-only interactive store viewer.
//!
//! Five tabs (Memories, Sessions, Profile, Imports, Stats) browse the local
//! store through the same read paths the MCP server and CLI use. No writes
//! beyond the access bookkeeping recall already performs, no network, no
//! provider calls — search runs the lexical recall path under the `null`
//! adapter so the viewer can never trigger an embedding request.
//!
//! Architecture: [`App`] holds all view state, [`draw`] and its per-tab
//! helpers are pure functions of `(Frame, &App)` (unit-tested against
//! `TestBackend`), and [`handle_key`] mutates `App` in response to one key.
//! Only [`run`] touches the real terminal: raw mode + alternate screen via
//! `ratatui::try_init`, restored on every exit path (including panics, via
//! ratatui's panic hook, and errors, via a drop guard).

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use memoryd_core::store::{
    ImportBatchItem, MemoryListItem, MemoryNeighborhood, ProfileFact, SessionListItem, Store,
    TableStats,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Rows fetched per `list_*_page` call; scrolling past the end appends a page.
const PAGE: usize = 50;
/// Search hits requested from the recall path.
const SEARCH_LIMIT: usize = 20;
/// Graph neighbors fetched for the detail view.
const NEIGHBOR_LIMIT: usize = 20;
/// Profile facts shown on the Profile tab.
const PROFILE_LIMIT: usize = 200;
/// Event-poll tick so the loop wakes up regularly without busy-spinning.
const TICK: Duration = Duration::from_millis(250);
/// How often auto-refresh re-reads the live stats panels while it is enabled.
const AUTO_REFRESH: Duration = Duration::from_secs(2);

/// Run the interactive viewer. Refuses to start when stdout is not a
/// terminal — the TUI is useless in a pipe and would emit control bytes.
pub(crate) fn run(cli: crate::Cli) -> Result<(), crate::CliError> {
    let cfg = cli.config()?;
    cfg.validate()?;
    if !std::io::stdout().is_terminal() {
        return Err(crate::CliError::TuiNotATerminal);
    }

    let store = Store::open(&cfg.db_path)?;
    let db_size_bytes = std::fs::metadata(&cfg.db_path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    let mut app = App::new(cfg.db_path.clone(), db_size_bytes);
    app.reload(&store);

    let mut terminal = ratatui::try_init()?;
    // Restore the terminal on every exit path: normal return, `?` errors in
    // the loop, and panics (ratatui::try_init installed a restoring hook).
    let _restore = RestoreOnDrop;
    event_loop(&mut terminal, &mut app, &store)
}

/// Drop guard that undoes raw mode + alternate screen.
struct RestoreOnDrop;

impl Drop for RestoreOnDrop {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    store: &Store,
) -> Result<(), crate::CliError> {
    loop {
        terminal.draw(|frame| draw(frame, app))?;
        if event::poll(TICK)?
            && let Event::Key(key) = event::read()?
            && handle_key(app, key, store) == Action::Quit
        {
            return Ok(());
        }
        if app.auto_refresh && app.last_refresh.elapsed() >= AUTO_REFRESH {
            app.refresh_stats(store);
        }
    }
}

/// What the event loop should do after a key was handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    None,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Memories,
    Sessions,
    Profile,
    Imports,
    Stats,
}

impl Tab {
    const ALL: [Self; 5] = [
        Self::Memories,
        Self::Sessions,
        Self::Profile,
        Self::Imports,
        Self::Stats,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Memories => "1 Memories",
            Self::Sessions => "2 Sessions",
            Self::Profile => "3 Profile",
            Self::Imports => "4 Imports",
            Self::Stats => "5 Stats",
        }
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|tab| *tab == self)
            .unwrap_or_default()
    }

    fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }
}

/// Search state on the Memories tab: `/` activates input, Enter submits to
/// the lexical recall path, Esc backs out of input and then results.
#[derive(Debug, Default)]
struct SearchState {
    active: bool,
    input: String,
    results: Option<Vec<SearchHit>>,
    selected: usize,
}

/// One search result row. `memory_id` is `Some` for durable-memory hits
/// (Enter opens the graph detail) and `None` for raw-event fallback hits.
#[derive(Debug, Clone, PartialEq)]
struct SearchHit {
    memory_id: Option<String>,
    kind: String,
    content: String,
    score: f64,
}

/// One level of the memory detail stack: a memory plus its one-hop graph
/// neighborhood. Timestamps/lifecycle are carried from the list row when
/// known; recentering on a neighbor only knows its lifecycle state.
#[derive(Debug)]
struct DetailView {
    hood: MemoryNeighborhood,
    lifecycle_state: Option<String>,
    created_at: Option<i64>,
    last_accessed_at: Option<i64>,
    selected: usize,
}

/// Session detail: list metadata plus the distilled narrative when a dream
/// run has distilled the session (the same text the MCP
/// `memory://session/{id}` resource serves). Raw per-session events have no
/// public read API in `Store`, so this view intentionally stops at
/// metadata + narrative.
#[derive(Debug)]
struct SessionDetail {
    item: SessionListItem,
    summary: Option<String>,
}

/// All TUI view state. Rendering reads it; [`handle_key`] mutates it.
struct App {
    tab: Tab,
    db_path: PathBuf,
    db_size_bytes: u64,
    memories: Vec<MemoryListItem>,
    memories_selected: usize,
    memories_end: bool,
    sessions: Vec<SessionListItem>,
    sessions_selected: usize,
    sessions_end: bool,
    profile: Vec<ProfileFact>,
    profile_selected: usize,
    imports: Vec<ImportBatchItem>,
    imports_selected: usize,
    stats: Vec<TableStats>,
    search: SearchState,
    detail: Vec<DetailView>,
    session_detail: Option<SessionDetail>,
    /// One-line error/status surfaced in the footer instead of crashing the UI.
    status: Option<String>,
    /// When true, the live stats panels re-read on the [`AUTO_REFRESH`] tick.
    auto_refresh: bool,
    /// When the stats panels were last re-read; drives auto-refresh and the
    /// "updated Ns ago" age shown on the Stats tab.
    last_refresh: Instant,
}

impl App {
    fn new(db_path: PathBuf, db_size_bytes: u64) -> Self {
        Self {
            tab: Tab::Memories,
            db_path,
            db_size_bytes,
            memories: Vec::new(),
            memories_selected: 0,
            memories_end: false,
            sessions: Vec::new(),
            sessions_selected: 0,
            sessions_end: false,
            profile: Vec::new(),
            profile_selected: 0,
            imports: Vec::new(),
            imports_selected: 0,
            stats: Vec::new(),
            search: SearchState::default(),
            detail: Vec::new(),
            session_detail: None,
            status: None,
            auto_refresh: true,
            last_refresh: Instant::now(),
        }
    }

    /// Load the first page of every tab. Read errors land in `status`.
    fn reload(&mut self, store: &Store) {
        self.extend_memories(store);
        self.extend_sessions(store);
        match store.active_profile_facts(PROFILE_LIMIT) {
            Ok(facts) => self.profile = facts,
            Err(err) => self.status = Some(format!("profile: {err}")),
        }
        match store.list_import_batches() {
            Ok(batches) => self.imports = batches,
            Err(err) => self.status = Some(format!("imports: {err}")),
        }
        match store.table_stats() {
            Ok(stats) => self.stats = stats,
            Err(err) => self.status = Some(format!("stats: {err}")),
        }
    }

    /// Re-read the lightweight global panels that change as the daemon works —
    /// table stats, import batches, and on-disk db size — without disturbing
    /// the browsing lists or their selection. Cheap enough for the auto-refresh
    /// tick.
    fn refresh_stats(&mut self, store: &Store) {
        match store.table_stats() {
            Ok(stats) => self.stats = stats,
            Err(err) => self.status = Some(format!("stats: {err}")),
        }
        match store.list_import_batches() {
            Ok(batches) => self.imports = batches,
            Err(err) => self.status = Some(format!("imports: {err}")),
        }
        self.imports_selected = self
            .imports_selected
            .min(self.imports.len().saturating_sub(1));
        self.db_size_bytes = std::fs::metadata(&self.db_path)
            .map(|meta| meta.len())
            .unwrap_or(self.db_size_bytes);
        self.last_refresh = Instant::now();
    }

    /// Manual full refresh (`r`): reload the browsing lists from the top —
    /// keeping how far the user had paged and clamping the selection — plus the
    /// profile, then the global stats panels. Any open memory/session detail is
    /// closed first: its cached neighborhood/narrative could be stale (or the
    /// row gone) after a refresh, so we drop back to the list and re-fetch on
    /// the next open rather than render outdated data.
    fn refresh(&mut self, store: &Store) {
        self.detail.clear();
        self.session_detail = None;
        let mem_limit = self.memories.len().max(PAGE);
        match store.list_memories_page(0, mem_limit) {
            Ok(batch) => {
                self.memories_end = batch.len() < mem_limit;
                self.memories = batch;
            }
            Err(err) => self.status = Some(format!("memories: {err}")),
        }
        self.memories_selected = self
            .memories_selected
            .min(self.memories.len().saturating_sub(1));
        let sess_limit = self.sessions.len().max(PAGE);
        match store.list_sessions_page(0, sess_limit) {
            Ok(batch) => {
                self.sessions_end = batch.len() < sess_limit;
                self.sessions = batch;
            }
            Err(err) => self.status = Some(format!("sessions: {err}")),
        }
        self.sessions_selected = self
            .sessions_selected
            .min(self.sessions.len().saturating_sub(1));
        match store.active_profile_facts(PROFILE_LIMIT) {
            Ok(facts) => self.profile = facts,
            Err(err) => self.status = Some(format!("profile: {err}")),
        }
        self.profile_selected = self
            .profile_selected
            .min(self.profile.len().saturating_sub(1));
        self.refresh_stats(store);
    }

    fn extend_memories(&mut self, store: &Store) {
        match store.list_memories_page(self.memories.len(), PAGE) {
            Ok(batch) => {
                self.memories_end = batch.len() < PAGE;
                self.memories.extend(batch);
            }
            Err(err) => self.status = Some(format!("memories: {err}")),
        }
    }

    fn extend_sessions(&mut self, store: &Store) {
        match store.list_sessions_page(self.sessions.len(), PAGE) {
            Ok(batch) => {
                self.sessions_end = batch.len() < PAGE;
                self.sessions.extend(batch);
            }
            Err(err) => self.status = Some(format!("sessions: {err}")),
        }
    }

    /// Esc: unwind one level of whatever is open.
    fn back(&mut self) {
        if !self.detail.is_empty() {
            self.detail.pop();
        } else if self.session_detail.is_some() {
            self.session_detail = None;
        } else if self.search.results.is_some() {
            self.search.results = None;
            self.search.input.clear();
            self.search.selected = 0;
        }
    }

    /// j/k/arrows: move the selection of whatever list currently has focus,
    /// fetching the next page when scrolling past the loaded end.
    fn move_selection(&mut self, down: bool, store: &Store) {
        if let Some(detail) = self.detail.last_mut() {
            detail.selected = step(detail.hood.neighbors.len(), detail.selected, down);
            return;
        }
        match self.tab {
            Tab::Memories => {
                if let Some(results) = &self.search.results {
                    self.search.selected = step(results.len(), self.search.selected, down);
                    return;
                }
                if down && self.memories_selected + 1 >= self.memories.len() && !self.memories_end {
                    self.extend_memories(store);
                }
                self.memories_selected = step(self.memories.len(), self.memories_selected, down);
            }
            Tab::Sessions => {
                if self.session_detail.is_some() {
                    return;
                }
                if down && self.sessions_selected + 1 >= self.sessions.len() && !self.sessions_end {
                    self.extend_sessions(store);
                }
                self.sessions_selected = step(self.sessions.len(), self.sessions_selected, down);
            }
            Tab::Profile => {
                self.profile_selected = step(self.profile.len(), self.profile_selected, down);
            }
            Tab::Imports => {
                self.imports_selected = step(self.imports.len(), self.imports_selected, down);
            }
            Tab::Stats => {}
        }
    }

    /// Enter: open the detail view for the selected row.
    fn open_selected(&mut self, store: &Store) {
        match self.tab {
            Tab::Memories => {
                if !self.detail.is_empty() {
                    return;
                }
                if let Some(results) = &self.search.results {
                    let Some(hit) = results.get(self.search.selected) else {
                        return;
                    };
                    match hit.memory_id.clone() {
                        Some(id) => self.push_detail(store, &id, None, None, None),
                        None => {
                            self.status =
                                Some("raw-event hit: no durable memory to open".to_string());
                        }
                    }
                    return;
                }
                let Some(item) = self.memories.get(self.memories_selected) else {
                    return;
                };
                let (id, lifecycle, created, accessed) = (
                    item.memory_id.clone(),
                    Some(item.lifecycle_state.clone()),
                    Some(item.created_at),
                    item.last_accessed_at,
                );
                self.push_detail(store, &id, lifecycle, created, accessed);
            }
            Tab::Sessions => {
                let Some(item) = self.sessions.get(self.sessions_selected) else {
                    return;
                };
                let summary = match store.distilled_session(&item.session_id) {
                    Ok(summary) => summary,
                    Err(err) => {
                        self.status = Some(format!("session: {err}"));
                        None
                    }
                };
                self.session_detail = Some(SessionDetail {
                    item: item.clone(),
                    summary,
                });
            }
            Tab::Profile | Tab::Imports | Tab::Stats => {}
        }
    }

    /// `g` inside a detail view: re-center the detail on the selected neighbor
    /// (pushed onto the stack so Esc walks back the visited path).
    fn recenter_detail(&mut self, store: &Store) {
        let Some(detail) = self.detail.last() else {
            return;
        };
        let Some(neighbor) = detail.hood.neighbors.get(detail.selected) else {
            return;
        };
        let (id, lifecycle) = (
            neighbor.memory_id.clone(),
            Some(neighbor.lifecycle_state.clone()),
        );
        self.push_detail(store, &id, lifecycle, None, None);
    }

    fn push_detail(
        &mut self,
        store: &Store,
        memory_id: &str,
        lifecycle_state: Option<String>,
        created_at: Option<i64>,
        last_accessed_at: Option<i64>,
    ) {
        match store.memory_neighbors(memory_id, NEIGHBOR_LIMIT) {
            Ok(Some(hood)) => self.detail.push(DetailView {
                hood,
                lifecycle_state,
                created_at,
                last_accessed_at,
                selected: 0,
            }),
            Ok(None) => {
                self.status = Some(format!("memory {memory_id} is not in a recallable state"));
            }
            Err(err) => self.status = Some(format!("detail: {err}")),
        }
    }

    /// Submit the search box to the same lexical recall the CLI `recall`
    /// command uses (durable memories first, raw-event fallback). The `null`
    /// adapter keeps this strictly lexical — the viewer never embeds.
    fn run_search(&mut self, store: &Store) {
        let query = self.search.input.trim().to_string();
        if query.is_empty() {
            self.search.results = None;
            return;
        }
        let args = crate::RecallArgs {
            query,
            limit: SEARCH_LIMIT,
            semantic: false,
            hops: 1,
            index_kind: None,
        };
        let adapter = memoryd_core::adapters::AdapterKind::from_default_adapter("null");
        match crate::recall_with_mode(store, &args, "brute-force", &adapter) {
            Ok(crate::RecallOutput::Memory(memory)) => {
                self.search.results = Some(
                    memory
                        .hits
                        .into_iter()
                        .map(|hit| SearchHit {
                            memory_id: Some(hit.memory_id),
                            kind: hit.kind,
                            content: hit.content,
                            score: hit.score,
                        })
                        .collect(),
                );
            }
            Ok(crate::RecallOutput::Event(event)) => {
                self.search.results = Some(
                    event
                        .hits
                        .into_iter()
                        .map(|hit| SearchHit {
                            memory_id: None,
                            kind: hit.kind,
                            content: hit.content,
                            score: hit.score,
                        })
                        .collect(),
                );
            }
            Err(err) => self.status = Some(format!("search: {err}")),
        }
        self.search.selected = 0;
    }
}

/// Clamped one-step list movement.
fn step(len: usize, selected: usize, down: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if down {
        (selected + 1).min(len - 1)
    } else {
        selected.saturating_sub(1)
    }
}

/// Apply one key press to the app. Pure state transition apart from the
/// read-only store queries it triggers (detail fetch, search, paging).
fn handle_key(app: &mut App, key: KeyEvent, store: &Store) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Action::Quit;
    }
    app.status = None;
    if app.search.active {
        match key.code {
            KeyCode::Esc => app.search.active = false,
            KeyCode::Enter => {
                app.run_search(store);
                app.search.active = false;
            }
            KeyCode::Backspace => {
                app.search.input.pop();
            }
            KeyCode::Char(c) => app.search.input.push(c),
            _ => {}
        }
        return Action::None;
    }
    match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Tab => {
            app.tab = app.tab.next();
            Action::None
        }
        KeyCode::Char(c @ '1'..='5') => {
            app.tab = Tab::ALL[(c as usize) - ('1' as usize)];
            Action::None
        }
        KeyCode::Char('/') if app.tab == Tab::Memories && app.detail.is_empty() => {
            app.search.active = true;
            Action::None
        }
        KeyCode::Esc => {
            app.back();
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.move_selection(true, store);
            Action::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.move_selection(false, store);
            Action::None
        }
        KeyCode::Enter => {
            app.open_selected(store);
            Action::None
        }
        KeyCode::Char('g') => {
            app.recenter_detail(store);
            Action::None
        }
        KeyCode::Char('r') => {
            app.refresh(store);
            Action::None
        }
        KeyCode::Char('a') => {
            app.auto_refresh = !app.auto_refresh;
            if app.auto_refresh {
                app.refresh_stats(store);
            }
            Action::None
        }
        _ => Action::None,
    }
}

/// Render one frame: tab bar, active tab body, key/status footer.
fn draw(frame: &mut Frame, app: &App) {
    let [tabs_area, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let tabs = Tabs::new(Tab::ALL.iter().map(|tab| tab.title()))
        .select(app.tab.index())
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_widget(tabs, tabs_area);

    match app.tab {
        Tab::Memories => draw_memories(frame, body, app),
        Tab::Sessions => draw_sessions(frame, body, app),
        Tab::Profile => draw_profile(frame, body, app),
        Tab::Imports => draw_imports(frame, body, app),
        Tab::Stats => draw_stats(frame, body, app),
    }

    draw_footer(frame, footer, app);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let keys = if app.search.active {
        "type query  Enter search  Esc cancel".to_string()
    } else if !app.detail.is_empty() {
        "j/k select neighbor  g recenter  Esc back  q quit".to_string()
    } else {
        format!(
            "Tab/1-5 tabs  j/k  Enter open  Esc back  / search  \
             r refresh  a auto:{}  q quit",
            if app.auto_refresh { "on" } else { "off" }
        )
    };
    let line = match &app.status {
        Some(status) => format!("{keys}  |  {status}"),
        None => keys,
    };
    frame.render_widget(
        Paragraph::new(truncate_chars(&line, area.width as usize)),
        area,
    );
}

fn draw_memories(frame: &mut Frame, area: Rect, app: &App) {
    if let Some(detail) = app.detail.last() {
        draw_memory_detail(frame, area, detail);
        return;
    }

    let searching = app.search.active || app.search.results.is_some();
    let list_area = if searching {
        let [input_area, rest] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
        let input = Paragraph::new(app.search.input.as_str())
            .block(Block::bordered().title("Search (lexical recall)"));
        frame.render_widget(input, input_area);
        rest
    } else {
        area
    };

    let width = row_width(list_area);
    if let Some(results) = &app.search.results {
        let rows = results.iter().map(|hit| {
            let origin = if hit.memory_id.is_some() {
                "memory"
            } else {
                "event"
            };
            row(
                &format!(
                    "{:>5.2}  {origin:<6} [{}] {}",
                    hit.score,
                    hit.kind,
                    single_line(&hit.content)
                ),
                width,
            )
        });
        render_list(
            frame,
            list_area,
            &format!("Results ({})", results.len()),
            rows,
            app.search.selected,
        );
        return;
    }

    let rows = app.memories.iter().map(|item| {
        row(
            &format!(
                "{}  {:<12} {:<10} {}",
                format_ts(item.created_at),
                item.kind,
                item.lifecycle_state,
                single_line(&item.content)
            ),
            width,
        )
    });
    render_list(
        frame,
        list_area,
        &format!("Memories ({} loaded)", app.memories.len()),
        rows,
        app.memories_selected,
    );
}

fn draw_memory_detail(frame: &mut Frame, area: Rect, detail: &DetailView) {
    let [meta_area, content_area, neighbors_area] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Min(3),
        Constraint::Length(8),
    ])
    .areas(area);

    let dash = "-".to_string();
    let meta = Paragraph::new(vec![
        Line::from(format!("id: {}", detail.hood.memory_id)),
        Line::from(format!(
            "kind: {}   lifecycle: {}",
            detail.hood.kind,
            detail.lifecycle_state.as_ref().unwrap_or(&dash)
        )),
        Line::from(format!(
            "created: {}   last_accessed: {}",
            detail
                .created_at
                .map(format_ts)
                .unwrap_or_else(|| dash.clone()),
            detail
                .last_accessed_at
                .map(format_ts)
                .unwrap_or_else(|| dash.clone())
        )),
    ])
    .block(Block::bordered().title("Memory"));
    frame.render_widget(meta, meta_area);

    let content = Paragraph::new(detail.hood.content.as_str())
        .wrap(Wrap { trim: false })
        .block(Block::bordered().title("Content"));
    frame.render_widget(content, content_area);

    let width = row_width(neighbors_area);
    let rows = detail.hood.neighbors.iter().map(|neighbor| {
        row(
            &format!(
                "{:<13} {:>4.2} [{}] {}",
                neighbor.link_type,
                neighbor.link_strength,
                neighbor.kind,
                single_line(&neighbor.content)
            ),
            width,
        )
    });
    render_list(
        frame,
        neighbors_area,
        &format!("Neighbors ({})", detail.hood.neighbors.len()),
        rows,
        detail.selected,
    );
}

fn draw_sessions(frame: &mut Frame, area: Rect, app: &App) {
    if let Some(detail) = &app.session_detail {
        let [meta_area, summary_area] =
            Layout::vertical([Constraint::Length(4), Constraint::Min(0)]).areas(area);
        let meta = Paragraph::new(vec![
            Line::from(format!("session: {}", detail.item.session_id)),
            Line::from(format!(
                "agent: {}   events: {}   started: {}",
                detail.item.agent,
                detail.item.event_count,
                format_ts(detail.item.started_at)
            )),
        ])
        .block(Block::bordered().title("Session"));
        frame.render_widget(meta, meta_area);

        let summary = detail
            .summary
            .as_deref()
            .unwrap_or("(no distilled narrative yet; raw events are not exposed read-only)");
        let body = Paragraph::new(summary)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title("Distilled narrative"));
        frame.render_widget(body, summary_area);
        return;
    }

    let width = row_width(area);
    let rows = app.sessions.iter().map(|item| {
        row(
            &format!(
                "{}  {:<10} {:>5} events  {}",
                format_ts(item.started_at),
                item.agent,
                item.event_count,
                item.session_id
            ),
            width,
        )
    });
    render_list(
        frame,
        area,
        &format!("Sessions ({} loaded)", app.sessions.len()),
        rows,
        app.sessions_selected,
    );
}

fn draw_profile(frame: &mut Frame, area: Rect, app: &App) {
    let width = row_width(area);
    let rows = app.profile.iter().map(|fact| {
        row(
            &format!(
                "{}: {}  [active {:.2}]",
                fact.fact_key,
                single_line(&fact.fact_value),
                fact.confidence
            ),
            width,
        )
    });
    render_list(
        frame,
        area,
        &format!("Profile facts ({})", app.profile.len()),
        rows,
        app.profile_selected,
    );
}

fn draw_imports(frame: &mut Frame, area: Rect, app: &App) {
    let width = row_width(area);
    let rows = app.imports.iter().map(|batch| {
        row(
            &format!(
                "{:<9} {:<9} {}/{} processed, {} skipped  {}",
                batch.state,
                batch.source,
                batch.processed,
                batch.total,
                batch.skipped,
                batch.path_or_uri
            ),
            width,
        )
    });
    render_list(
        frame,
        area,
        &format!("Import batches ({})", app.imports.len()),
        rows,
        app.imports_selected,
    );
}

fn draw_stats(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines = vec![
        Line::from(format!("db_path: {}", app.db_path.display())),
        Line::from(format!(
            "db_size: {} ({} bytes)",
            human_size(app.db_size_bytes),
            app.db_size_bytes
        )),
        Line::from(format!(
            "auto-refresh: {}   ·   r refresh now, a toggle   ·   updated {}s ago",
            if app.auto_refresh { "on (2s)" } else { "off" },
            app.last_refresh.elapsed().as_secs()
        )),
        Line::from(""),
    ];
    for stat in &app.stats {
        lines.push(Line::from(format!("{}: {}", stat.table, stat.rows)));
    }
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title("Stats")),
        area,
    );
}

/// Render a bordered, stateful list with the house highlight style.
fn render_list<'a>(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    rows: impl Iterator<Item = ListItem<'a>>,
    selected: usize,
) {
    let list = List::new(rows)
        .block(Block::bordered().title(title.to_string()))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

/// Usable row width inside a bordered list (borders + highlight symbol).
fn row_width(area: Rect) -> usize {
    (area.width as usize).saturating_sub(4)
}

fn row(text: &str, width: usize) -> ListItem<'static> {
    ListItem::new(truncate_chars(text, width))
}

/// Collapse a multi-line content blob into one list row.
fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to at most `max` characters on a char boundary, marking the cut
/// with an ellipsis. Never slices inside a multi-byte character.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

fn human_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Render unix milliseconds as UTC `YYYY-MM-DD HH:MM` with pure integer math
/// (no chrono): days-to-civil from Howard Hinnant's algorithm.
fn format_ts(unix_ms: i64) -> String {
    let secs = unix_ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Convert days since 1970-01-01 to a (year, month, day) civil date
/// (proleptic Gregorian, valid across the i64 day range we can ever store).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use memoryd_core::store::{MemoryNeighbor, NewRawEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "memoryd-tui-{name}-{}-{nanos}.db",
            std::process::id()
        ))
    }

    fn cleanup_db_files(path: &std::path::Path) {
        for suffix in ["", "-shm", "-wal"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn render(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("test terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                if let Some(cell) = buffer.cell((x, y)) {
                    text.push_str(cell.symbol());
                }
            }
            text.push('\n');
        }
        text
    }

    fn fixture_app() -> App {
        let mut app = App::new(PathBuf::from("/tmp/fixture.db"), 4096);
        app.memories = vec![
            MemoryListItem {
                memory_id: "mem-1".to_string(),
                kind: "decision".to_string(),
                lifecycle_state: "active".to_string(),
                content: "use sqlite WAL for the store".to_string(),
                created_at: 1_700_000_000_000,
                last_accessed_at: None,
            },
            MemoryListItem {
                memory_id: "mem-2".to_string(),
                kind: "observation".to_string(),
                lifecycle_state: "associated".to_string(),
                content: "tests run under TestBackend".to_string(),
                created_at: 1_700_000_100_000,
                last_accessed_at: Some(1_700_000_200_000),
            },
        ];
        app
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn renders_memories_tab_list_and_footer() {
        let app = fixture_app();
        let text = render(&app);

        assert!(text.contains("1 Memories"), "tab bar missing: {text}");
        assert!(text.contains("Memories (2 loaded)"), "title: {text}");
        assert!(
            text.contains("use sqlite WAL for the store"),
            "row content: {text}"
        );
        assert!(text.contains("2023-11-14 22:13"), "row timestamp: {text}");
        assert!(text.contains("q quit"), "footer keys: {text}");
    }

    #[test]
    fn renders_memory_detail_with_neighbors() {
        let mut app = fixture_app();
        app.detail.push(DetailView {
            hood: MemoryNeighborhood {
                memory_id: "mem-1".to_string(),
                kind: "decision".to_string(),
                content: "use sqlite WAL for the store".to_string(),
                neighbors: vec![MemoryNeighbor {
                    memory_id: "mem-2".to_string(),
                    kind: "observation".to_string(),
                    content: "tests run under TestBackend".to_string(),
                    link_type: "co_occurrence".to_string(),
                    link_strength: 0.42,
                    last_reinforced_at: 1_700_000_100_000,
                    lifecycle_state: "associated".to_string(),
                }],
            },
            lifecycle_state: Some("active".to_string()),
            created_at: Some(1_700_000_000_000),
            last_accessed_at: None,
            selected: 0,
        });

        let text = render(&app);
        assert!(text.contains("id: mem-1"), "meta id: {text}");
        assert!(
            text.contains("kind: decision   lifecycle: active"),
            "meta kind/lifecycle: {text}"
        );
        assert!(
            text.contains("use sqlite WAL for the store"),
            "content: {text}"
        );
        assert!(text.contains("Neighbors (1)"), "neighbor list: {text}");
        assert!(text.contains("co_occurrence"), "link type: {text}");
        assert!(
            text.contains("tests run under TestBackend"),
            "neighbor content: {text}"
        );
        assert!(text.contains("g recenter"), "detail footer: {text}");
    }

    #[test]
    fn renders_stats_tab_counts() {
        let mut app = fixture_app();
        app.tab = Tab::Stats;
        app.stats = vec![
            TableStats {
                table: "memories".to_string(),
                rows: 3,
            },
            TableStats {
                table: "raw_events".to_string(),
                rows: 7,
            },
        ];

        let text = render(&app);
        assert!(text.contains("5 Stats"), "tab bar: {text}");
        assert!(text.contains("db_path: /tmp/fixture.db"), "db path: {text}");
        assert!(text.contains("4.0 KiB"), "db size: {text}");
        assert!(text.contains("memories: 3"), "table count: {text}");
        assert!(text.contains("raw_events: 7"), "table count: {text}");
        assert!(
            text.contains("auto-refresh: on"),
            "auto-refresh line: {text}"
        );
    }

    #[test]
    fn auto_refresh_defaults_on_toggle_and_manual_refresh_reloads() {
        let path = temp_db_path("tui-refresh");
        let mut store = Store::open(&path).expect("store opens");
        capture(&mut store, "s1", 1_700_000_000_000, "first remembered note");
        dream(&mut store, &path);

        let mut app = App::new(path.clone(), 0);
        assert!(app.auto_refresh, "auto-refresh defaults on");
        assert!(app.stats.is_empty(), "stats are unread before any refresh");

        // Manual refresh ('r') reads the lists and the stats panels.
        handle_key(&mut app, key(KeyCode::Char('r')), &store);
        assert!(!app.stats.is_empty(), "manual refresh reads stats");
        let memories_after_refresh = app.memories.len();
        assert!(memories_after_refresh > 0, "manual refresh loads memories");

        // 'a' toggles auto-refresh off, then back on.
        handle_key(&mut app, key(KeyCode::Char('a')), &store);
        assert!(!app.auto_refresh, "a toggles auto-refresh off");
        handle_key(&mut app, key(KeyCode::Char('a')), &store);
        assert!(app.auto_refresh, "a toggles auto-refresh back on");

        // New data lands on the next refresh without dropping existing rows.
        capture(
            &mut store,
            "s1",
            1_700_000_100_000,
            "second remembered note",
        );
        dream(&mut store, &path);
        handle_key(&mut app, key(KeyCode::Char('r')), &store);
        assert!(
            app.memories.len() >= memories_after_refresh,
            "refresh reflects newly consolidated memories"
        );

        // Opening a memory detail then refreshing drops back to the list, since
        // a cached detail can go stale (or its row vanish) after a refresh.
        handle_key(&mut app, key(KeyCode::Enter), &store);
        assert!(!app.detail.is_empty(), "Enter opens a memory detail");
        handle_key(&mut app, key(KeyCode::Char('r')), &store);
        assert!(app.detail.is_empty(), "refresh closes stale detail views");

        cleanup_db_files(&path);
    }

    #[test]
    fn search_mode_updates_input_and_returns_results_state() {
        let path = temp_db_path("search");
        let mut store = Store::open(&path).expect("store opens");
        store
            .capture_event(NewRawEvent {
                session_id: "tui-test".to_string(),
                agent: "test".to_string(),
                source: "test".to_string(),
                kind: "note".to_string(),
                payload: serde_json::json!({ "text": "the zebra crossed the road" }),
                provenance: serde_json::json!({}),
                ts_ms: 1_700_000_000_000,
            })
            .expect("capture seeds the store");

        let mut app = App::new(path.clone(), 0);
        app.reload(&store);

        assert_eq!(
            handle_key(&mut app, key(KeyCode::Char('/')), &store),
            Action::None
        );
        assert!(app.search.active, "/ enters search mode");
        for c in "zebra".chars() {
            handle_key(&mut app, key(KeyCode::Char(c)), &store);
        }
        assert_eq!(app.search.input, "zebra");

        handle_key(&mut app, key(KeyCode::Enter), &store);
        assert!(!app.search.active, "Enter leaves input mode");
        let results = app.search.results.as_ref().expect("results populated");
        assert_eq!(results.len(), 1, "one lexical hit: {results:?}");
        assert!(results[0].content.contains("zebra"));

        // The rendered frame shows the query and the hit.
        let text = render(&app);
        assert!(text.contains("zebra"), "search render: {text}");
        assert!(text.contains("Results (1)"), "results title: {text}");

        // Esc clears results back to the plain list.
        handle_key(&mut app, key(KeyCode::Esc), &store);
        assert!(app.search.results.is_none());
        assert!(app.search.input.is_empty());

        cleanup_db_files(&path);
    }

    #[test]
    fn quits_on_q_and_ctrl_c_and_switches_tabs() {
        let path = temp_db_path("keys");
        let store = Store::open(&path).expect("store opens");
        let mut app = App::new(path.clone(), 0);

        assert_eq!(
            handle_key(&mut app, key(KeyCode::Char('q')), &store),
            Action::Quit
        );
        assert_eq!(
            handle_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &store
            ),
            Action::Quit
        );

        handle_key(&mut app, key(KeyCode::Char('4')), &store);
        assert_eq!(app.tab, Tab::Imports);
        handle_key(&mut app, key(KeyCode::Tab), &store);
        assert_eq!(app.tab, Tab::Stats);
        handle_key(&mut app, key(KeyCode::Tab), &store);
        assert_eq!(app.tab, Tab::Memories, "tab cycles back around");

        cleanup_db_files(&path);
    }

    #[test]
    fn format_ts_renders_utc() {
        assert_eq!(format_ts(0), "1970-01-01 00:00");
        assert_eq!(format_ts(1_700_000_000_000), "2023-11-14 22:13");
        // Negative (pre-epoch) values floor toward earlier time, not panic.
        assert_eq!(format_ts(-1), "1969-12-31 23:59");
        // Leap-year day.
        assert_eq!(format_ts(1_709_164_800_000), "2024-02-29 00:00");
    }

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate_chars("short", 10), "short");
        assert_eq!(truncate_chars("exactly-10", 10), "exactly-10");
        assert_eq!(truncate_chars("0123456789ab", 10), "012345678…");
        // Multi-byte characters are kept whole, never sliced mid-codepoint.
        assert_eq!(truncate_chars("ééééé", 3), "éé…");
        assert_eq!(truncate_chars("anything", 0), "…");
    }

    fn capture(store: &mut Store, session: &str, ts_ms: i64, text: &str) {
        store
            .capture_event(NewRawEvent {
                session_id: session.to_string(),
                agent: "claude".to_string(),
                source: "test".to_string(),
                kind: "observation".to_string(),
                payload: serde_json::json!({ "text": text }),
                provenance: serde_json::json!({}),
                ts_ms,
            })
            .expect("capture succeeds");
    }

    /// Consolidate captured events into durable memories the same way the
    /// `dream` command does (default config adapter, manual trigger).
    fn dream(store: &mut Store, db_path: &std::path::Path) {
        let cfg = memoryd_core::config::Config::with_db_path(db_path.to_path_buf());
        let adapter = memoryd_core::adapters::AdapterKind::from_default_adapter(
            &cfg.providers.default_adapter,
        );
        let opts = memoryd_core::dream::DreamOptions {
            trigger: "manual",
            budget_usd: cfg.caps.paid_spend_cap_usd,
            max_seconds: cfg.caps.dream_wallclock_secs,
        };
        memoryd_core::dream::dream_once(store, &adapter, &cfg.caps, &opts, &|| {
            crate::unix_ms_now()
        })
        .expect("dream succeeds");
    }

    #[test]
    fn enter_opens_memory_detail_and_g_recenters_on_neighbors() {
        let path = temp_db_path("detail-flow");
        let mut store = Store::open(&path).expect("store opens");
        capture(&mut store, "s1", 1_000, "wal busy timeout fix");
        capture(&mut store, "s1", 1_001, "vacuum schedule weekly");
        dream(&mut store, &path);

        let mut app = App::new(path.clone(), 0);
        app.reload(&store);
        assert!(app.memories.len() >= 2, "dream consolidated the captures");

        // Enter opens the selected memory's one-hop neighborhood.
        handle_key(&mut app, key(KeyCode::Enter), &store);
        assert_eq!(app.detail.len(), 1);
        assert_eq!(app.detail[0].hood.memory_id, app.memories[0].memory_id);
        assert!(
            !app.detail[0].hood.neighbors.is_empty(),
            "same-session siblings are linked"
        );

        // Enter inside a detail view is a no-op (the stack is already open).
        handle_key(&mut app, key(KeyCode::Enter), &store);
        assert_eq!(app.detail.len(), 1);

        // j/k move the neighbor selection, clamped to the list.
        handle_key(&mut app, key(KeyCode::Char('j')), &store);
        assert!(app.detail[0].selected < app.detail[0].hood.neighbors.len());
        handle_key(&mut app, key(KeyCode::Char('k')), &store);
        assert_eq!(app.detail[0].selected, 0);

        // g recenters on the selected neighbor, pushing a second level.
        handle_key(&mut app, key(KeyCode::Char('g')), &store);
        assert_eq!(app.detail.len(), 2);
        assert_eq!(
            app.detail[1].hood.memory_id,
            app.detail[0].hood.neighbors[0].memory_id
        );

        // Esc walks the visited path back one level at a time.
        handle_key(&mut app, key(KeyCode::Esc), &store);
        assert_eq!(app.detail.len(), 1);
        handle_key(&mut app, key(KeyCode::Esc), &store);
        assert!(app.detail.is_empty());

        // g at the top level (no detail open) is a no-op.
        handle_key(&mut app, key(KeyCode::Char('g')), &store);
        assert!(app.detail.is_empty());

        cleanup_db_files(&path);
    }

    #[test]
    fn search_returns_durable_memory_hits_and_opens_detail() {
        let path = temp_db_path("search-memory");
        let mut store = Store::open(&path).expect("store opens");
        capture(&mut store, "s1", 1_000, "vacuum schedule weekly");
        capture(&mut store, "s1", 1_001, "wal busy timeout fix");
        dream(&mut store, &path);

        let mut app = App::new(path.clone(), 0);
        app.reload(&store);

        handle_key(&mut app, key(KeyCode::Char('/')), &store);
        for c in "vacuum".chars() {
            handle_key(&mut app, key(KeyCode::Char(c)), &store);
        }
        // The search-input footer renders while typing.
        let typing = render(&app);
        assert!(
            typing.contains("type query  Enter search  Esc cancel"),
            "search footer: {typing}"
        );

        handle_key(&mut app, key(KeyCode::Enter), &store);
        let hit_id = {
            let results = app.search.results.as_ref().expect("results populated");
            assert!(!results.is_empty(), "lexical hit over durable memories");
            results[0]
                .memory_id
                .clone()
                .expect("durable memory hit carries an id")
        };

        // Durable hits render with the "memory" origin tag.
        let rendered = render(&app);
        assert!(rendered.contains("memory ["), "origin tag: {rendered}");

        // j keeps the selection within the result list.
        handle_key(&mut app, key(KeyCode::Char('j')), &store);
        let len = app.search.results.as_ref().expect("results").len();
        assert!(app.search.selected < len);
        app.search.selected = 0;

        // Enter opens the hit's graph detail; Esc backs out to the results,
        // then clears them.
        handle_key(&mut app, key(KeyCode::Enter), &store);
        assert_eq!(app.detail.len(), 1);
        assert_eq!(app.detail[0].hood.memory_id, hit_id);
        handle_key(&mut app, key(KeyCode::Esc), &store);
        assert!(app.detail.is_empty());
        assert!(app.search.results.is_some());
        handle_key(&mut app, key(KeyCode::Esc), &store);
        assert!(app.search.results.is_none());

        // Submitting an empty query clears results instead of searching.
        handle_key(&mut app, key(KeyCode::Char('/')), &store);
        handle_key(&mut app, key(KeyCode::Enter), &store);
        assert!(app.search.results.is_none());

        // A token-free query is a recall error, surfaced in the footer status.
        handle_key(&mut app, key(KeyCode::Char('/')), &store);
        for c in "!!!".chars() {
            handle_key(&mut app, key(KeyCode::Char(c)), &store);
        }
        handle_key(&mut app, key(KeyCode::Enter), &store);
        assert!(
            app.status
                .as_deref()
                .is_some_and(|s| s.starts_with("search:")),
            "status: {:?}",
            app.status
        );

        cleanup_db_files(&path);
    }

    #[test]
    fn open_selected_surfaces_missing_memory_and_raw_event_hits() {
        let path = temp_db_path("open-missing");
        let store = Store::open(&path).expect("store opens");
        let mut app = fixture_app();

        // The fixture rows are not in the store: Enter surfaces a status line.
        handle_key(&mut app, key(KeyCode::Enter), &store);
        assert!(app.detail.is_empty());
        assert!(
            app.status
                .as_deref()
                .expect("status set")
                .contains("not in a recallable state")
        );

        // A raw-event search hit has no durable memory to open.
        app.search.results = Some(vec![SearchHit {
            memory_id: None,
            kind: "note".to_string(),
            content: "raw event content".to_string(),
            score: 1.0,
        }]);
        handle_key(&mut app, key(KeyCode::Enter), &store);
        assert_eq!(
            app.status.as_deref(),
            Some("raw-event hit: no durable memory to open")
        );

        // Raw-event hits render with the "event" origin tag.
        let rendered = render(&app);
        assert!(rendered.contains("event  ["), "origin tag: {rendered}");

        // Selection past the end of the results is a no-op.
        app.search.selected = 5;
        handle_key(&mut app, key(KeyCode::Enter), &store);
        assert!(app.detail.is_empty());

        // Enter on an empty memories list is a no-op too.
        app.search.results = None;
        app.search.selected = 0;
        app.memories.clear();
        handle_key(&mut app, key(KeyCode::Enter), &store);
        assert!(app.detail.is_empty() && app.status.is_none());

        // Enter on an empty sessions list, Profile, and Stats does nothing.
        app.tab = Tab::Sessions;
        handle_key(&mut app, key(KeyCode::Enter), &store);
        assert!(app.session_detail.is_none());
        app.tab = Tab::Profile;
        handle_key(&mut app, key(KeyCode::Enter), &store);
        app.tab = Tab::Stats;
        handle_key(&mut app, key(KeyCode::Enter), &store);
        assert!(app.detail.is_empty() && app.session_detail.is_none());

        cleanup_db_files(&path);
    }

    #[test]
    fn sessions_tab_opens_detail_and_esc_closes() {
        let path = temp_db_path("sessions-flow");
        let mut store = Store::open(&path).expect("store opens");
        capture(&mut store, "sess-a", 1_000, "alpha work");
        capture(&mut store, "sess-b", 2_000, "beta work");

        let mut app = App::new(path.clone(), 0);
        app.reload(&store);
        assert_eq!(app.sessions.len(), 2);
        app.tab = Tab::Sessions;

        let list = render(&app);
        assert!(list.contains("2 Sessions"), "tab bar: {list}");
        assert!(list.contains("Sessions (2 loaded)"), "title: {list}");

        // j selects the second session; Enter opens its detail.
        handle_key(&mut app, key(KeyCode::Char('j')), &store);
        assert_eq!(app.sessions_selected, 1);
        let expected = app.sessions[1].session_id.clone();
        handle_key(&mut app, key(KeyCode::Enter), &store);
        {
            let detail = app.session_detail.as_ref().expect("session detail");
            assert_eq!(detail.item.session_id, expected);
            assert!(detail.summary.is_none(), "nothing distilled yet");
        }

        // j/k are inert while the detail is open.
        handle_key(&mut app, key(KeyCode::Char('j')), &store);
        assert_eq!(app.sessions_selected, 1);

        // The undistilled detail renders the placeholder narrative.
        let rendered = render(&app);
        assert!(
            rendered.contains(&format!("session: {expected}")),
            "{rendered}"
        );
        assert!(
            rendered.contains("no distilled narrative yet"),
            "placeholder: {rendered}"
        );

        // Esc closes the detail back to the list.
        handle_key(&mut app, key(KeyCode::Esc), &store);
        assert!(app.session_detail.is_none());

        // A distilled narrative renders verbatim.
        app.session_detail = Some(SessionDetail {
            item: app.sessions[0].clone(),
            summary: Some("Fixed the WAL bug.".to_string()),
        });
        let distilled = render(&app);
        assert!(distilled.contains("Fixed the WAL bug."), "{distilled}");

        cleanup_db_files(&path);
    }

    #[test]
    fn renders_profile_and_imports_tabs_with_selection() {
        let path = temp_db_path("profile-imports");
        let store = Store::open(&path).expect("store opens");
        let mut app = fixture_app();

        app.tab = Tab::Profile;
        app.profile = vec![
            ProfileFact {
                fact_key: "editor".to_string(),
                fact_value: "helix".to_string(),
                confidence: 0.9,
            },
            ProfileFact {
                fact_key: "shell".to_string(),
                fact_value: "fish".to_string(),
                confidence: 0.75,
            },
        ];
        let profile = render(&app);
        assert!(profile.contains("3 Profile"), "tab bar: {profile}");
        assert!(profile.contains("Profile facts (2)"), "title: {profile}");
        assert!(
            profile.contains("editor: helix  [active 0.90]"),
            "fact row: {profile}"
        );
        handle_key(&mut app, key(KeyCode::Char('j')), &store);
        assert_eq!(app.profile_selected, 1);
        handle_key(&mut app, key(KeyCode::Char('k')), &store);
        assert_eq!(app.profile_selected, 0);

        app.tab = Tab::Imports;
        app.imports = vec![ImportBatchItem {
            source: "claude-session".to_string(),
            path_or_uri: "/tmp/x.jsonl".to_string(),
            total: 4,
            processed: 3,
            skipped: 1,
            state: "completed".to_string(),
        }];
        let imports = render(&app);
        assert!(imports.contains("4 Imports"), "tab bar: {imports}");
        assert!(imports.contains("Import batches (1)"), "title: {imports}");
        assert!(
            imports.contains("3/4 processed, 1 skipped"),
            "batch row: {imports}"
        );
        handle_key(&mut app, key(KeyCode::Char('j')), &store);
        assert_eq!(app.imports_selected, 0, "single row clamps in place");

        // Stats has no selection: j/k are inert.
        app.tab = Tab::Stats;
        handle_key(&mut app, key(KeyCode::Char('j')), &store);

        cleanup_db_files(&path);
    }

    #[test]
    fn footer_shows_status_message() {
        let mut app = fixture_app();
        app.status = Some("search: boom".to_string());
        let text = render(&app);
        assert!(text.contains("|  search: boom"), "footer: {text}");
    }

    #[test]
    fn scrolling_past_loaded_end_fetches_next_page_and_clamps() {
        let path = temp_db_path("paging");
        let store = Store::open(&path).expect("store opens");
        let mut app = fixture_app();

        // The fixture rows look like a partial page; scrolling past the end
        // asks the (empty) store for more and learns it is the end.
        assert!(!app.memories_end);
        app.memories_selected = 1;
        handle_key(&mut app, key(KeyCode::Char('j')), &store);
        assert!(app.memories_end);
        assert_eq!(app.memories_selected, 1, "selection clamps at the end");
        handle_key(&mut app, key(KeyCode::Down), &store);
        assert_eq!(app.memories_selected, 1);
        handle_key(&mut app, key(KeyCode::Up), &store);
        assert_eq!(app.memories_selected, 0);

        // Sessions paging follows the same pattern.
        app.tab = Tab::Sessions;
        assert!(!app.sessions_end);
        handle_key(&mut app, key(KeyCode::Char('j')), &store);
        assert!(app.sessions_end);
        assert_eq!(app.sessions_selected, 0, "empty list clamps to zero");

        cleanup_db_files(&path);
    }

    #[test]
    fn handle_key_ignores_release_events_and_unmapped_keys() {
        let path = temp_db_path("edge-keys");
        let store = Store::open(&path).expect("store opens");
        let mut app = fixture_app();

        // Key releases never mutate state or quit.
        let mut release = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(handle_key(&mut app, release, &store), Action::None);

        // '/' only starts a search on the Memories tab.
        app.tab = Tab::Sessions;
        handle_key(&mut app, key(KeyCode::Char('/')), &store);
        assert!(!app.search.active);
        app.tab = Tab::Memories;

        // Unmapped keys at the top level are ignored.
        assert_eq!(
            handle_key(&mut app, key(KeyCode::Home), &store),
            Action::None
        );

        // In search mode: Backspace edits, unmapped keys are ignored, Esc
        // leaves input mode with the draft intact.
        handle_key(&mut app, key(KeyCode::Char('/')), &store);
        assert!(app.search.active);
        handle_key(&mut app, key(KeyCode::Char('a')), &store);
        handle_key(&mut app, key(KeyCode::Char('b')), &store);
        handle_key(&mut app, key(KeyCode::Backspace), &store);
        assert_eq!(app.search.input, "a");
        handle_key(&mut app, key(KeyCode::Home), &store);
        assert_eq!(app.search.input, "a");
        handle_key(&mut app, key(KeyCode::Esc), &store);
        assert!(!app.search.active);
        assert_eq!(app.search.input, "a");

        cleanup_db_files(&path);
    }

    #[test]
    fn recenter_needs_a_neighbor_that_is_in_the_store() {
        let path = temp_db_path("recenter-edge");
        let store = Store::open(&path).expect("store opens");
        let mut app = fixture_app();
        app.detail.push(DetailView {
            hood: MemoryNeighborhood {
                memory_id: "mem-1".to_string(),
                kind: "decision".to_string(),
                content: "use sqlite WAL for the store".to_string(),
                neighbors: Vec::new(),
            },
            lifecycle_state: None,
            created_at: None,
            last_accessed_at: None,
            selected: 0,
        });

        // No neighbors: g and j/k are inert inside the detail.
        handle_key(&mut app, key(KeyCode::Char('g')), &store);
        assert_eq!(app.detail.len(), 1);
        handle_key(&mut app, key(KeyCode::Char('j')), &store);
        assert_eq!(app.detail[0].selected, 0);

        // A neighbor that is not in the store surfaces a status, not a push.
        app.detail[0].hood.neighbors.push(MemoryNeighbor {
            memory_id: "mem-ghost".to_string(),
            kind: "observation".to_string(),
            content: "gone".to_string(),
            link_type: "co_occurrence".to_string(),
            link_strength: 0.5,
            last_reinforced_at: 0,
            lifecycle_state: "active".to_string(),
        });
        handle_key(&mut app, key(KeyCode::Char('g')), &store);
        assert_eq!(app.detail.len(), 1);
        assert!(
            app.status
                .as_deref()
                .expect("status set")
                .contains("not in a recallable state")
        );

        cleanup_db_files(&path);
    }

    #[test]
    fn step_clamps_and_human_size_picks_units() {
        assert_eq!(step(0, 0, true), 0);
        assert_eq!(step(0, 5, false), 0);
        assert_eq!(step(3, 0, true), 1);
        assert_eq!(step(3, 2, true), 2);
        assert_eq!(step(3, 0, false), 0);
        assert_eq!(step(3, 2, false), 1);

        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2 * 1024), "2.0 KiB");
        assert_eq!(human_size(3 * 1024 * 1024 / 2), "1.5 MiB");
        assert_eq!(human_size(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }
}
