//! Full-screen TUI for browsing directories and previewing cleanup targets.

use crate::app::{execute_removal, scan_mode_from_cli};
use crate::cleanup::{
    RemovalKind, RemovalTarget, ScanConfig, ScanMode, TargetContentStats,
    collect_cleanup_targets_fast, format_size, summarize_target_contents,
};
use crate::cli::Cli;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use std::{
    collections::HashMap,
    env,
    error::Error,
    fs, io,
    io::Write,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::Instant,
};

const COLOR_ACCENT: Color = Color::Rgb(10, 132, 255);
const COLOR_ACCENT_ALT: Color = Color::Rgb(90, 200, 250);
const COLOR_HIGHLIGHT_BG: Color = Color::Rgb(56, 84, 135);
const COLOR_HIGHLIGHT_FG: Color = Color::Rgb(245, 247, 250);
const COLOR_SUCCESS: Color = Color::Rgb(48, 209, 88);
const COLOR_WARNING: Color = Color::Rgb(172, 172, 178);
const COLOR_MUTED: Color = Color::Rgb(99, 99, 102);
const COLOR_TEXT_SOFT: Color = Color::Rgb(242, 242, 247);
const COLOR_SCROLL_TRACK: Color = Color::Rgb(72, 72, 74);
const COLOR_BORDER: Color = Color::Rgb(72, 72, 74);

/// Launches the interactive directory browser and preview UI.
pub(crate) fn run_interactive(cli: Cli) -> Result<(), Box<dyn Error>> {
    let mut state = TuiState::new(Some(prompt_initial_browser_dir()?));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_tui_loop(&mut terminal, &cli, &mut state);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

/// A selectable root entry shown in the root-selection dialog.
struct BrowserRoot {
    label: String,
    path: PathBuf,
}

impl BrowserRoot {
    /// Creates a new root entry with a display label.
    fn new(label: String, path: PathBuf) -> Self {
        Self { label, path }
    }
}

/// Actions shown in the browse list.
enum BrowserAction {
    /// Start scanning the current directory.
    UseCurrent,
    /// Move to the parent directory.
    GoUp,
    /// Open the root-selection dialog.
    ChangeRoot,
    /// Enter the provided child directory.
    Enter(PathBuf),
    /// Exit the TUI.
    Quit,
}

/// Top-level screens within the TUI.
enum TuiMode {
    /// Directory browsing screen.
    Browse,
    /// Root-selection modal.
    RootSelect,
    /// Scan preview screen.
    Preview,
}

/// Actions emitted by the preview key handler.
enum PreviewAction {
    /// No action is required.
    None,
    /// Return to the browse screen.
    Back,
    /// Execute the pending cleanup.
    Clean,
    /// Exit the application.
    Quit,
}

/// Current state of preview scanning and sizing work.
enum PreviewStatus {
    /// Target discovery is still in progress.
    Scanning { started_at: Instant },
    /// Targets are known and their sizes are being computed.
    CalculatingSize {
        started_at: Instant,
        target_count: usize,
        sized_count: usize,
    },
    /// The preview is fully ready for review and cleanup.
    Ready,
    /// Preview generation stopped with an error-like condition.
    Failed(String),
}

/// Messages sent from the background preview worker to the UI thread.
enum PreviewScanUpdate {
    /// Initial target discovery has completed.
    TargetsFound {
        scan_id: u64,
        targets: Vec<RemovalTarget>,
        scan_elapsed: std::time::Duration,
    },
    /// A batch of target sizes has been computed.
    SizeBatch {
        scan_id: u64,
        sizes: Vec<(usize, u64)>,
        sized_count: usize,
        running_total_size: u64,
        is_complete: bool,
        total_elapsed: std::time::Duration,
    },
}

/// Messages sent from the selected-entry stats worker to the UI thread.
enum SelectedStatsUpdate {
    /// Content counts for the currently selected target are ready.
    Ready {
        request_id: u64,
        path: PathBuf,
        stats: TargetContentStats,
    },
}

/// Mutable state for the entire interactive session.
struct TuiState {
    mode: TuiMode,
    current_dir: PathBuf,
    last_dir: Option<PathBuf>,
    browser_actions: Vec<BrowserAction>,
    browser_index: usize,
    roots: Vec<BrowserRoot>,
    root_index: usize,
    preview_targets: Vec<RemovalTarget>,
    preview_index: usize,
    preview_total_size: u64,
    preview_found_elapsed: std::time::Duration,
    preview_scan_elapsed: std::time::Duration,
    preview_status: PreviewStatus,
    preview_scan_rx: Option<Receiver<PreviewScanUpdate>>,
    preview_scan_id: u64,
    preview_scan_mode: ScanMode,
    selected_stats_cache: HashMap<PathBuf, TargetContentStats>,
    selected_stats_rx: Option<Receiver<SelectedStatsUpdate>>,
    selected_stats_request_id: u64,
    selected_stats_path: Option<PathBuf>,
}

impl TuiState {
    /// Creates a new TUI state with the provided initial directory.
    fn new(initial_dir: Option<PathBuf>) -> Self {
        let current_dir = initial_dir.unwrap_or_else(|| PathBuf::from("/"));
        Self {
            mode: TuiMode::Browse,
            current_dir,
            last_dir: None,
            browser_actions: Vec::new(),
            browser_index: 0,
            roots: Vec::new(),
            root_index: 0,
            preview_targets: Vec::new(),
            preview_index: 0,
            preview_total_size: 0,
            preview_found_elapsed: std::time::Duration::from_secs(0),
            preview_scan_elapsed: std::time::Duration::from_secs(0),
            preview_status: PreviewStatus::Ready,
            preview_scan_rx: None,
            preview_scan_id: 0,
            preview_scan_mode: ScanMode::All,
            selected_stats_cache: HashMap::new(),
            selected_stats_rx: None,
            selected_stats_request_id: 0,
            selected_stats_path: None,
        }
    }
}

/// Runs the TUI event loop until the user exits.
fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    cli: &Cli,
    state: &mut TuiState,
) -> Result<(), Box<dyn Error>> {
    state.roots = browser_roots(state.last_dir.as_deref());

    loop {
        process_preview_scan(state);
        process_selected_stats(state);
        request_selected_stats_if_needed(state);
        refresh_browser_actions(state)?;
        terminal.draw(|frame| draw_tui(frame, state))?;

        if !event::poll(std::time::Duration::from_millis(100))? {
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match state.mode {
                TuiMode::Browse => {
                    if handle_browse_key(key.code, state, cli)? {
                        return Ok(());
                    }
                }
                TuiMode::RootSelect => {
                    if handle_root_key(key.code, state)? {
                        return Ok(());
                    }
                }
                TuiMode::Preview => match handle_preview_key(key.code, state)? {
                    PreviewAction::None => {}
                    PreviewAction::Back => cancel_preview(state),
                    PreviewAction::Quit => return Ok(()),
                    PreviewAction::Clean => {
                        disable_raw_mode()?;
                        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                        terminal.show_cursor()?;

                        let _ = execute_removal(
                            &state.preview_targets,
                            false,
                            cli.quiet,
                            cli.no_progress,
                            cli.progress_style,
                        );

                        execute!(terminal.backend_mut(), EnterAlternateScreen)?;
                        enable_raw_mode()?;
                        state.last_dir = Some(state.current_dir.clone());
                        cancel_preview(state);
                    }
                },
            }
        }
    }
}

/// Handles key input while browsing directories.
fn handle_browse_key(key: KeyCode, state: &mut TuiState, cli: &Cli) -> Result<bool, Box<dyn Error>> {
    match key {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Down | KeyCode::Char('j') => {
            if !state.browser_actions.is_empty() {
                state.browser_index = (state.browser_index + 1) % state.browser_actions.len();
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if !state.browser_actions.is_empty() {
                state.browser_index = if state.browser_index == 0 {
                    state.browser_actions.len() - 1
                } else {
                    state.browser_index - 1
                };
            }
        }
        KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
            if let Some(parent) = state.current_dir.parent() {
                state.current_dir = parent.to_path_buf();
                state.browser_index = 0;
            }
        }
        KeyCode::Char('r') => {
            state.roots = browser_roots(Some(&state.current_dir));
            state.root_index = 0;
            state.mode = TuiMode::RootSelect;
        }
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => match state.browser_actions.get(state.browser_index) {
            Some(BrowserAction::UseCurrent) => build_preview(state, cli)?,
            Some(BrowserAction::GoUp) => {
                if let Some(parent) = state.current_dir.parent() {
                    state.current_dir = parent.to_path_buf();
                    state.browser_index = 0;
                }
            }
            Some(BrowserAction::ChangeRoot) => {
                state.roots = browser_roots(Some(&state.current_dir));
                state.root_index = 0;
                state.mode = TuiMode::RootSelect;
            }
            Some(BrowserAction::Enter(path)) => {
                state.current_dir = path.clone();
                state.browser_index = 0;
            }
            Some(BrowserAction::Quit) => return Ok(true),
            None => {}
        },
        _ => {}
    }
    Ok(false)
}

/// Handles key input while selecting a root directory.
fn handle_root_key(key: KeyCode, state: &mut TuiState) -> Result<bool, Box<dyn Error>> {
    match key {
        KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('b') => {
            state.mode = TuiMode::Browse
        }
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Down | KeyCode::Char('j') => {
            if !state.roots.is_empty() {
                state.root_index = (state.root_index + 1) % state.roots.len();
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if !state.roots.is_empty() {
                state.root_index = if state.root_index == 0 {
                    state.roots.len() - 1
                } else {
                    state.root_index - 1
                };
            }
        }
        KeyCode::Enter => {
            if let Some(root) = state.roots.get(state.root_index) {
                state.current_dir = root.path.clone();
                state.browser_index = 0;
                state.mode = TuiMode::Browse;
            }
        }
        _ => {}
    }
    Ok(false)
}

/// Handles key input while reviewing the preview list.
fn handle_preview_key(key: KeyCode, state: &mut TuiState) -> Result<PreviewAction, Box<dyn Error>> {
    match key {
        KeyCode::Char('q') => return Ok(PreviewAction::Quit),
        KeyCode::Esc | KeyCode::Char('b') => return Ok(PreviewAction::Back),
        KeyCode::Char('c') => {
            if matches!(state.preview_status, PreviewStatus::Ready) && !state.preview_targets.is_empty() {
                return Ok(PreviewAction::Clean);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if !state.preview_targets.is_empty() {
                state.preview_index = (state.preview_index + 1) % state.preview_targets.len();
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if !state.preview_targets.is_empty() {
                state.preview_index = if state.preview_index == 0 {
                    state.preview_targets.len() - 1
                } else {
                    state.preview_index - 1
                };
            }
        }
        _ => {}
    }
    Ok(PreviewAction::None)
}

/// Starts a background scan for the current directory and enters preview mode.
fn build_preview(state: &mut TuiState, cli: &Cli) -> Result<(), Box<dyn Error>> {
    let requested_mode = scan_mode_from_cli(cli);
    let mode = match requested_mode {
        ScanMode::All => ScanMode::DirectoriesOnly,
        other => other,
    };
    let config = ScanConfig::new(&cli.exclude, mode)?;
    let path = state.current_dir.clone();
    let scan_id = state.preview_scan_id.wrapping_add(1);
    let (tx, rx) = mpsc::channel();

    state.preview_scan_id = scan_id;
    state.preview_scan_rx = Some(rx);
    state.preview_targets.clear();
    state.preview_index = 0;
    state.preview_total_size = 0;
    state.preview_found_elapsed = std::time::Duration::from_secs(0);
    state.preview_scan_elapsed = std::time::Duration::from_secs(0);
    state.preview_scan_mode = mode;
    state.selected_stats_cache.clear();
    state.selected_stats_rx = None;
    state.selected_stats_path = None;
    state.preview_status = PreviewStatus::Scanning {
        started_at: Instant::now(),
    };
    state.mode = TuiMode::Preview;

    thread::spawn(move || {
        let scan_start = Instant::now();
        let targets = collect_cleanup_targets_fast(&path, &config);

        if tx
            .send(PreviewScanUpdate::TargetsFound {
                scan_id,
                targets: targets.clone(),
                scan_elapsed: scan_start.elapsed(),
            })
            .is_err()
        {
            return;
        }

        let mut running_total_size = 0u64;
        let mut batch = Vec::new();
        let batch_size = 16usize;

        for (index, target) in targets.iter().enumerate() {
            let size = target.compute_size();
            running_total_size += size;
            batch.push((index, size));

            let is_last = index + 1 == targets.len();
            if batch.len() >= batch_size || is_last {
                let sized_count = index + 1;
                if tx
                    .send(PreviewScanUpdate::SizeBatch {
                        scan_id,
                        sizes: std::mem::take(&mut batch),
                        sized_count,
                        running_total_size,
                        is_complete: is_last,
                        total_elapsed: scan_start.elapsed(),
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
    });

    Ok(())
}

/// Pulls pending preview-scan updates into UI state.
fn process_preview_scan(state: &mut TuiState) {
    let mut disconnected = false;
    let mut updates = Vec::new();

    if let Some(rx) = state.preview_scan_rx.as_ref() {
        loop {
            match rx.try_recv() {
                Ok(update) => updates.push(update),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
    }

    for update in updates {
        match update {
            PreviewScanUpdate::TargetsFound {
                scan_id,
                targets,
                scan_elapsed,
            } if scan_id == state.preview_scan_id => {
                state.preview_targets = targets;
                state.preview_found_elapsed = scan_elapsed;
                state.preview_scan_elapsed = scan_elapsed;
                state.preview_index = 0;
                state.preview_status = PreviewStatus::CalculatingSize {
                    started_at: Instant::now(),
                    target_count: state.preview_targets.len(),
                    sized_count: 0,
                };
            }
            PreviewScanUpdate::SizeBatch {
                scan_id,
                sizes,
                sized_count,
                running_total_size,
                is_complete,
                total_elapsed,
            } if scan_id == state.preview_scan_id => {
                for (index, size) in sizes {
                    if let Some(target) = state.preview_targets.get_mut(index) {
                        target.set_size(size);
                    }
                }
                state.preview_total_size = running_total_size;
                state.preview_scan_elapsed = total_elapsed;
                if is_complete {
                    state.preview_status = PreviewStatus::Ready;
                    state.preview_scan_rx = None;
                } else {
                    state.preview_status = PreviewStatus::CalculatingSize {
                        started_at: Instant::now(),
                        target_count: state.preview_targets.len(),
                        sized_count,
                    };
                }
            }
            _ => {}
        }
    }

    if disconnected && !matches!(state.preview_status, PreviewStatus::Ready) {
        state.preview_scan_rx = None;
        state.preview_status = PreviewStatus::Failed("Scan stopped unexpectedly.".to_string());
    }
}

/// Pulls pending selected-target content counts into UI state.
fn process_selected_stats(state: &mut TuiState) {
    let mut updates = Vec::new();

    if let Some(rx) = state.selected_stats_rx.as_ref() {
        loop {
            match rx.try_recv() {
                Ok(update) => updates.push(update),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    for update in updates {
        match update {
            SelectedStatsUpdate::Ready {
                request_id,
                path,
                stats,
            } if request_id == state.selected_stats_request_id => {
                state.selected_stats_cache.insert(path.clone(), stats);
                if state.selected_stats_path.as_ref() == Some(&path) {
                    state.selected_stats_rx = None;
                }
            }
            _ => {}
        }
    }
}

/// Starts a background stats request for the currently selected preview target.
fn request_selected_stats_if_needed(state: &mut TuiState) {
    let Some(entry) = state.preview_targets.get(state.preview_index) else {
        state.selected_stats_rx = None;
        state.selected_stats_path = None;
        return;
    };

    if !supports_selected_stats(entry.kind()) {
        state.selected_stats_rx = None;
        state.selected_stats_path = None;
        return;
    }

    let path = entry.path().to_path_buf();
    if state.selected_stats_cache.contains_key(&path)
        || state.selected_stats_path.as_ref() == Some(&path)
    {
        return;
    }

    let request_id = state.selected_stats_request_id.wrapping_add(1);
    let target = entry.clone();
    let path_clone = path.clone();
    let (tx, rx) = mpsc::channel();

    state.selected_stats_request_id = request_id;
    state.selected_stats_path = Some(path);
    state.selected_stats_rx = Some(rx);

    thread::spawn(move || {
        let stats = summarize_target_contents(&target);
        let _ = tx.send(SelectedStatsUpdate::Ready {
            request_id,
            path: path_clone,
            stats,
        });
    });
}

/// Returns whether a target kind should display file and folder counts.
fn supports_selected_stats(kind: RemovalKind) -> bool {
    matches!(kind, RemovalKind::Directory | RemovalKind::LogDirectory)
}

/// Leaves preview mode and clears any pending preview state.
fn cancel_preview(state: &mut TuiState) {
    state.preview_scan_rx = None;
    state.preview_targets.clear();
    state.preview_index = 0;
    state.preview_total_size = 0;
    state.preview_found_elapsed = std::time::Duration::from_secs(0);
    state.preview_scan_elapsed = std::time::Duration::from_secs(0);
    state.preview_scan_mode = ScanMode::All;
    state.selected_stats_cache.clear();
    state.selected_stats_rx = None;
    state.selected_stats_path = None;
    state.preview_status = PreviewStatus::Ready;
    state.mode = TuiMode::Browse;
}

/// Rebuilds the browse-list actions for the current directory.
fn refresh_browser_actions(state: &mut TuiState) -> Result<(), Box<dyn Error>> {
    if !matches!(state.mode, TuiMode::Browse) {
        return Ok(());
    }

    let directories = list_subdirectories(&state.current_dir)?;
    let mut actions = Vec::new();
    actions.push(BrowserAction::UseCurrent);
    if state.current_dir.parent().is_some() {
        actions.push(BrowserAction::GoUp);
    }
    actions.push(BrowserAction::ChangeRoot);
    actions.extend(directories.into_iter().map(BrowserAction::Enter));
    actions.push(BrowserAction::Quit);
    state.browser_actions = actions;
    if state.browser_index >= state.browser_actions.len() {
        state.browser_index = state.browser_actions.len().saturating_sub(1);
    }
    Ok(())
}

/// Draws the active TUI screen for the current mode.
fn draw_tui(frame: &mut Frame, state: &TuiState) {
    match state.mode {
        TuiMode::Browse => draw_browse_view(frame, state),
        TuiMode::RootSelect => draw_root_view(frame, state),
        TuiMode::Preview => draw_preview_view(frame, state),
    }
}

/// Draws the main directory-browsing screen.
fn draw_browse_view(frame: &mut Frame, state: &TuiState) {
    let shell = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(10), Constraint::Length(3)])
        .split(frame.area());
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(shell[1]);

    render_header(frame, shell[0], "dust", "Browse", &state.current_dir, "Ready");

    let items: Vec<ListItem> = state
        .browser_actions
        .iter()
        .map(|action| {
            ListItem::new(browser_action_label(
                action,
                &state.current_dir,
                inner_width(body[0]).saturating_sub(4),
            ))
        })
        .collect();

    let mut list_state = ListState::default();
    list_state.select(Some(state.browser_index));
    let list = List::new(items)
        .block(panel_block("Directories"))
        .highlight_style(
            Style::default()
                .fg(COLOR_HIGHLIGHT_FG)
                .bg(COLOR_HIGHLIGHT_BG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    frame.render_stateful_widget(list, body[0], &mut list_state);

    render_browse_sidebar(frame, body[1], state);
    render_action_footer(
        frame,
        shell[2],
        "Enter: preview   r: roots   Backspace/←: parent   q: quit",
    );
}

/// Draws the modal root-selection screen on top of the browse view.
fn draw_root_view(frame: &mut Frame, state: &TuiState) {
    draw_browse_view(frame, state);
    let area = centered_rect(72, 62, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block("Select Root"), area);

    let inner = area.inner(Margin {
        vertical: 1,
        horizontal: 2,
    });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(6), Constraint::Length(1)])
        .split(inner);

    let title = Paragraph::new(vec![Line::from(Span::styled(
        "Choose a starting point",
        Style::default()
            .fg(COLOR_TEXT_SOFT)
            .add_modifier(Modifier::DIM),
    ))]);
    frame.render_widget(title, chunks[0]);

    let items: Vec<ListItem> = state
        .roots
        .iter()
        .map(|root| ListItem::new(truncate_middle(&root.label, inner_width(chunks[1]).saturating_sub(2))))
        .collect();
    let mut list_state = ListState::default();
    list_state.select(Some(state.root_index));
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(COLOR_HIGHLIGHT_FG)
                .bg(COLOR_HIGHLIGHT_BG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    frame.render_stateful_widget(list, chunks[1], &mut list_state);

    let footer = Paragraph::new(Line::from(vec![
        Span::styled("keys ", Style::default().fg(COLOR_WARNING)),
        Span::raw("Enter: use root   Esc/b/Backspace/←: back   q: quit"),
    ]))
    .wrap(Wrap { trim: true });
    frame.render_widget(footer, chunks[2]);
}

/// Draws the cleanup preview screen with targets and details.
fn draw_preview_view(frame: &mut Frame, state: &TuiState) {
    let shell = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(10), Constraint::Length(3)])
        .split(frame.area());
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(shell[1]);
    let list_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(7)])
        .split(body[0]);

    render_header(
        frame,
        shell[0],
        "dust",
        "Preview",
        &state.current_dir,
        &preview_status_line(state),
    );

    let preview_path = Paragraph::new(Line::from(truncate_middle(
        &display_path(&state.current_dir),
        inner_width(list_chunks[0]).saturating_sub(1),
    )))
    .block(panel_block("Path"))
    .wrap(Wrap { trim: true });
    frame.render_widget(preview_path, list_chunks[0]);

    let items: Vec<ListItem> = state
        .preview_targets
        .iter()
        .map(|entry| {
            ListItem::new(format_preview_entry_lines(
                entry,
                inner_width(list_chunks[1]).saturating_sub(4),
            ))
        })
        .collect();
    let mut list_state = ListState::default();
    if !state.preview_targets.is_empty() {
        list_state.select(Some(state.preview_index));
    }
    let list = List::new(items)
        .block(panel_block("Items to Remove"))
        .highlight_style(
            Style::default()
                .fg(COLOR_HIGHLIGHT_FG)
                .bg(COLOR_HIGHLIGHT_BG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    frame.render_stateful_widget(list, list_chunks[1], &mut list_state);
    render_preview_scrollbar(frame, list_chunks[1], state);

    render_preview_sidebar(frame, body[1], state);
    render_action_footer(
        frame,
        shell[2],
        "↑/↓ or j/k: scroll   c: clean   Esc/b: back   q: quit",
    );
}

/// Draws the scrollbar for the preview target list.
fn render_preview_scrollbar(frame: &mut Frame, area: Rect, state: &TuiState) {
    if state.preview_targets.is_empty() {
        return;
    }

    let item_height = if inner_width(area).saturating_sub(4) <= 48 {
        2
    } else {
        1
    };
    let viewport_items = inner_height(area).max(1) / item_height;
    let mut scrollbar_state = ScrollbarState::new(state.preview_targets.len())
        .position(state.preview_index)
        .viewport_content_length(viewport_items.max(1));
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_style(Style::default().fg(COLOR_ACCENT_ALT))
        .track_style(Style::default().fg(COLOR_SCROLL_TRACK))
        .begin_symbol(None)
        .end_symbol(None);

    frame.render_stateful_widget(scrollbar, area, &mut scrollbar_state);
}

/// Renders the shared header with mode, path, and status information.
fn render_header(frame: &mut Frame, area: Rect, app: &str, mode: &str, path: &Path, status: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    render_segmented_control(frame, chunks[0], if mode == "Preview" { 1 } else { 0 });

    let toolbar = Paragraph::new(Line::from(vec![
        Span::styled(app, Style::default().fg(COLOR_ACCENT).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(mode, Style::default().fg(COLOR_ACCENT_ALT).add_modifier(Modifier::DIM)),
        Span::raw("   "),
        Span::styled("•", Style::default().fg(COLOR_MUTED)),
        Span::raw("  "),
        Span::raw(truncate_middle(
            &display_path(path),
            inner_width(chunks[1]).saturating_sub(34),
        )),
        Span::raw("  "),
        Span::styled("•", Style::default().fg(COLOR_MUTED)),
        Span::raw("  "),
        Span::styled("Status", Style::default().fg(COLOR_SUCCESS)),
        Span::raw(" "),
        Span::raw(truncate_middle(
            status,
            inner_width(chunks[1]).saturating_sub(48),
        )),
    ]))
    .block(separator_block(Borders::BOTTOM))
    .wrap(Wrap { trim: true });
    frame.render_widget(toolbar, chunks[1]);
}

/// Renders the informational sidebar for browse mode.
fn render_browse_sidebar(frame: &mut Frame, area: Rect, state: &TuiState) {
    let details = Paragraph::new(vec![
        Line::from(Span::styled(
            "Summary",
            Style::default().fg(COLOR_ACCENT_ALT).add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("Folder: {}", file_name_or_path(&state.current_dir))),
        Line::from(format!("Entries: {}", state.browser_actions.len().saturating_sub(4))),
        Line::from(format!(
            "Parent folder: {}",
            if state.current_dir.parent().is_some() { "available" } else { "none" }
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Keys",
            Style::default().fg(COLOR_WARNING).add_modifier(Modifier::BOLD),
        )),
        Line::from("Use arrows or j/k to move."),
        Line::from("Press Enter to open a folder."),
        Line::from("Choose [Scan here] to preview this folder."),
        Line::from("Press r to switch roots."),
        Line::from("Backspace, ←, or h goes up."),
    ])
    .block(panel_block("Info"))
    .wrap(Wrap { trim: true });
    frame.render_widget(details, area);
}

/// Renders the informational sidebar for preview mode.
fn render_preview_sidebar(frame: &mut Frame, area: Rect, state: &TuiState) {
    let selected = state.preview_targets.get(state.preview_index);
    let lines = if let Some(entry) = selected {
        let mut lines = vec![
            Line::from(Span::styled(
                "Selected",
                Style::default().fg(COLOR_ACCENT_ALT).add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("Type: {}", selected_type_label(entry))),
            Line::from(format!("Size: {}", target_size_text(entry))),
        ];
        if supports_selected_stats(entry.kind()) {
            let (file_count_line, dir_count_line) = selected_directory_stats_lines(state, entry);
            lines.push(file_count_line);
            lines.push(dir_count_line);
        }
        lines.extend([
            Line::from(truncate_middle(
                &display_path(entry.path()),
                inner_width(area).saturating_sub(1),
            )),
            Line::from(""),
        ]);
        lines.extend(scan_summary_lines(state, inner_width(area).saturating_sub(1)));
        lines.extend([
            Line::from(Span::styled(
                "Keys",
                Style::default().fg(COLOR_WARNING).add_modifier(Modifier::BOLD),
            )),
            Line::from("Review the planned targets."),
            Line::from("Use ↑/↓ or j/k to inspect each entry."),
            Line::from("Press c to remove these items."),
            Line::from("Press Esc or b to go back."),
        ]);
        lines
    } else {
        let mut lines = vec![
            Line::from(Span::styled(
                "Selected",
                Style::default().fg(COLOR_ACCENT_ALT).add_modifier(Modifier::BOLD),
            )),
            Line::from("Type: -"),
            Line::from("Size: -"),
            Line::from("Nothing to remove yet."),
            Line::from(""),
        ];
        lines.extend(scan_summary_lines(state, inner_width(area).saturating_sub(1)));
        lines.extend([
            Line::from(Span::styled(
                "Keys",
                Style::default().fg(COLOR_WARNING).add_modifier(Modifier::BOLD),
            )),
            Line::from("Review the planned targets."),
            Line::from("Use ↑/↓ or j/k to inspect each entry."),
            Line::from("Press c to remove these items."),
            Line::from("Press Esc or b to go back."),
        ]);
        lines
    };
    frame.render_widget(
        Paragraph::new(lines).block(panel_block("Info")).wrap(Wrap { trim: true }),
        area,
    );
}

/// Renders the footer that lists available key bindings.
fn render_action_footer(frame: &mut Frame, area: Rect, text: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("keys ", Style::default().fg(COLOR_WARNING)),
            Span::raw(text),
        ]))
        .block(separator_block(Borders::TOP))
        .wrap(Wrap { trim: true }),
        area,
    );
}

/// Renders the browse/preview segmented control in the header.
fn render_segmented_control(frame: &mut Frame, area: Rect, selected: usize) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled("[ ", Style::default().fg(COLOR_BORDER)),
            Span::styled("Browse", segmented_text_style(selected == 0)),
            Span::styled(" | ", Style::default().fg(COLOR_BORDER)),
            Span::styled("Preview", segmented_text_style(selected == 1)),
            Span::styled(" ]", Style::default().fg(COLOR_BORDER)),
        ]))
        .wrap(Wrap { trim: true }),
        area,
    );
}

/// Returns the text style for a segmented-control tab.
fn segmented_text_style(active: bool) -> Style {
    if active {
        Style::default()
            .fg(COLOR_HIGHLIGHT_FG)
            .bg(COLOR_ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(COLOR_TEXT_SOFT)
    }
}

/// Builds a rounded panel block with the shared visual style.
fn panel_block<'a>(title: &'a str) -> Block<'a> {
    Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(COLOR_ACCENT_ALT).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(COLOR_BORDER))
}

/// Builds a simple separator block for header or footer rows.
fn separator_block(borders: Borders) -> Block<'static> {
    Block::default()
        .borders(borders)
        .border_style(Style::default().fg(COLOR_BORDER))
}

/// Formats the short header status text for preview mode.
fn preview_status_line(state: &TuiState) -> String {
    match &state.preview_status {
        PreviewStatus::Scanning { started_at } => format!("scanning • {:.1?}", started_at.elapsed()),
        PreviewStatus::CalculatingSize {
            started_at,
            target_count,
            sized_count,
        } => format!(
            "{sized_count}/{target_count} sized • {:.1?}",
            started_at.elapsed(),
        ),
        PreviewStatus::Ready => format!(
            "{} target(s) • {} • scan {:.2?}",
            state.preview_targets.len(),
            format_size(state.preview_total_size),
            state.preview_scan_elapsed
        ),
        PreviewStatus::Failed(message) => format!("scan failed • {}", message),
    }
}

/// Builds the scan summary lines shown in the preview sidebar.
fn scan_summary_lines(state: &TuiState, max_width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        "Scan",
        Style::default().fg(COLOR_SUCCESS).add_modifier(Modifier::BOLD),
    ))];

    match &state.preview_status {
        PreviewStatus::Scanning { started_at } => {
            lines.push(Line::from(truncate_middle(
                &format!("Looking for items… {:.1?}", started_at.elapsed()),
                max_width,
            )));
            lines.push(Line::from("The list will appear as soon as scanning finishes."));
        }
        PreviewStatus::CalculatingSize {
            started_at,
            target_count,
            sized_count,
        } => {
            lines.push(Line::from(truncate_middle(
                &format!("Found {target_count} item(s) in {:.1?}", state.preview_found_elapsed),
                max_width,
            )));
            lines.push(Line::from(truncate_middle(
                &format!("Sized {sized_count}/{target_count}… {:.1?}", started_at.elapsed()),
                max_width,
            )));
        }
        PreviewStatus::Ready => {
            lines.push(Line::from(truncate_middle(
                &format!("Found {} item(s) in {:.1?}", state.preview_targets.len(), state.preview_found_elapsed),
                max_width,
            )));
            lines.push(Line::from(truncate_middle(
                &format!("Total size ready in {:.1?}", state.preview_scan_elapsed),
                max_width,
            )));
        }
        PreviewStatus::Failed(message) => {
            lines.push(Line::from(truncate_middle(message, max_width)));
        }
    }

    lines.push(Line::from(""));
    lines
}

/// Formats the current target size or a placeholder while it is still pending.
fn target_size_text(entry: &RemovalTarget) -> String {
    entry
        .size_bytes()
        .map(format_size)
        .unwrap_or_else(|| "Calculating…".to_string())
}

/// Returns the display label for the selected target type.
fn selected_type_label(entry: &RemovalTarget) -> &'static str {
    match entry.kind() {
        RemovalKind::Directory | RemovalKind::LogDirectory => "DIR",
        RemovalKind::FileGroup => "FILES",
    }
}

/// Builds the file and folder count lines for the selected directory target.
fn selected_directory_stats_lines(
    state: &TuiState,
    entry: &RemovalTarget,
) -> (Line<'static>, Line<'static>) {
    if let Some(stats) = state.selected_stats_cache.get(entry.path()) {
        (
            Line::from(format!("Files: {}", format_count(stats.file_count))),
            Line::from(format!("Folders: {}", format_count(stats.dir_count))),
        )
    } else {
        (
            Line::from("Files: Calculating…"),
            Line::from("Folders: Calculating…"),
        )
    }
}

/// Formats a count with thousands separators.
fn format_count(value: usize) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);

    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            formatted.push(',');
        }
        formatted.push(ch);
    }

    formatted.chars().rev().collect()
}

/// Formats a browse action into the list text shown to the user.
fn browser_action_label(action: &BrowserAction, current_dir: &Path, max_width: usize) -> String {
    match action {
        BrowserAction::UseCurrent => {
            truncate_middle(&format!("[Scan here] {}", display_path(current_dir)), max_width)
        }
        BrowserAction::GoUp => "[Up] ..".to_string(),
        BrowserAction::ChangeRoot => "[Roots]".to_string(),
        BrowserAction::Enter(path) => truncate_middle(&format!("{}/", file_name_or_path(path)), max_width),
        BrowserAction::Quit => "[Quit]".to_string(),
    }
}

/// Formats a preview entry using either single-line or compact multi-line layout.
fn format_preview_entry_lines(entry: &RemovalTarget, max_width: usize) -> Vec<Line<'static>> {
    if max_width <= 48 {
        return format_preview_entry_compact(entry, max_width);
    }
    vec![Line::from(format_preview_entry_single_line(entry, max_width))]
}

/// Formats a preview entry for narrow layouts.
fn format_preview_entry_compact(entry: &RemovalTarget, max_width: usize) -> Vec<Line<'static>> {
    let label_line =
        truncate_middle(&format!("[{}] {}", entry.label(), display_path(entry.path())), max_width);
    let size_line = truncate_middle(&format!("size: {}", target_size_text(entry)), max_width);
    vec![Line::from(label_line), Line::from(size_line)]
}

/// Formats a preview entry as a single line with aligned size information.
fn format_preview_entry_single_line(entry: &RemovalTarget, max_width: usize) -> String {
    let size = format!("({})", target_size_text(entry));
    let prefix = format!("[{}] ", entry.label());
    if max_width <= prefix.len() + size.len() + 1 {
        return truncate_middle(&format!("{prefix}{}", display_path(entry.path())), max_width);
    }
    let path_width = max_width.saturating_sub(prefix.len() + size.len() + 1);
    let path = truncate_middle(&display_path(entry.path()), path_width);
    format!("{prefix}{path} {size}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_stats_are_requested_for_directory_like_targets() {
        assert!(supports_selected_stats(RemovalKind::Directory));
        assert!(supports_selected_stats(RemovalKind::LogDirectory));
        assert!(!supports_selected_stats(RemovalKind::FileGroup));
    }
}

/// Builds the list of root entries available to the browse UI.
fn browser_roots(last_dir: Option<&Path>) -> Vec<BrowserRoot> {
    let mut roots = Vec::new();
    if let Some(path) = last_dir {
        push_browser_root(&mut roots, BrowserRoot::new(format!("Last: {}", path.display()), path.to_path_buf()));
    }
    if let Ok(current_dir) = env::current_dir() {
        push_browser_root(
            &mut roots,
            BrowserRoot::new(format!("Current: {}", current_dir.display()), current_dir),
        );
    }
    if let Some(home_dir) = home_dir() {
        push_browser_root(&mut roots, BrowserRoot::new(format!("Home: {}", home_dir.display()), home_dir));
    }
    for root in platform_roots() {
        push_browser_root(&mut roots, root);
    }
    roots
}

/// Adds a root entry if it has not already been inserted.
fn push_browser_root(roots: &mut Vec<BrowserRoot>, root: BrowserRoot) {
    if !roots.iter().any(|existing| existing.path == root.path) {
        roots.push(root);
    }
}

/// Lists the direct child directories of a path in stable display order.
fn list_subdirectories(path: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut directories = fs::read_dir(path)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|entry_path| entry_path.is_dir())
        .collect::<Vec<_>>();

    directories.sort_by_key(|entry| file_name_or_path(entry).to_ascii_lowercase().replace('\\', "/"));
    Ok(directories)
}

/// Returns the file name of a path, falling back to the full path string.
fn file_name_or_path(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

/// Prompts the user for the initial directory before entering the TUI.
fn prompt_initial_browser_dir() -> Result<PathBuf, Box<dyn Error>> {
    loop {
        let fallback = env::current_dir()?;
        print!("Enter initial directory [{}]: ", fallback.display());
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Ok(fallback);
        }

        let candidate = PathBuf::from(trimmed.trim_matches('"'));
        if candidate.is_dir() {
            return Ok(candidate);
        }

        eprintln!("Directory not found or not a directory: {}", candidate.display());
    }
}

/// Returns the user's home directory on the current platform.
fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        env::var_os("HOME").map(PathBuf::from)
    }
}

#[cfg(windows)]
/// Returns the available drive roots on Windows.
fn platform_roots() -> Vec<BrowserRoot> {
    let mut roots = Vec::new();
    for letter in 'A'..='Z' {
        let path = PathBuf::from(format!("{letter}:\\"));
        if path.exists() {
            roots.push(BrowserRoot::new(format!("Drive {letter}:\\"), path));
        }
    }
    roots
}

#[cfg(not(windows))]
/// Returns the root entry used on Unix-like platforms.
fn platform_roots() -> Vec<BrowserRoot> {
    vec![BrowserRoot::new("Root: /".to_string(), PathBuf::from("/"))]
}

/// Converts a path to its display string.
fn display_path(path: &Path) -> String {
    path.display().to_string()
}

/// Truncates text in the middle so both the head and tail remain visible.
fn truncate_middle(text: &str, max_width: usize) -> String {
    let len = text.chars().count();
    if len <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }
    let head_len = (max_width - 3) / 2;
    let tail_len = max_width - 3 - head_len;
    let head: String = text.chars().take(head_len).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(tail_len)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{head}...{tail}")
}

/// Returns the inner width of a block after borders are removed.
fn inner_width(area: Rect) -> usize {
    area.width.saturating_sub(2) as usize
}

/// Returns the inner height of a block after borders are removed.
fn inner_height(area: Rect) -> usize {
    area.height.saturating_sub(2) as usize
}

/// Builds a centered rectangle sized by percentages of the parent area.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
