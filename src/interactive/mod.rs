//! Full-screen TUI for browsing directories and previewing cleanup targets.

use crate::app::{execute_removal, scan_mode_from_cli};
use crate::cleanup::{
    RemovalKind, RemovalTarget, ScanConfig, ScanMode, TargetContentStats,
    collect_cleanup_targets_fast, compute_target_size_for_path, format_size,
    summarize_target_contents_for_path,
};
use crate::cli::Cli;
use crate::update::{self, UpdateInstall, UpdateNotice, UpdateProgress};
use arboard::Clipboard;
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
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
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
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
pub(crate) fn run_interactive(
    cli: Cli,
    update_notice: Option<UpdateNotice>,
) -> Result<(), Box<dyn Error>> {
    let mut state = TuiState::new(env::current_dir().ok(), update_notice);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_tui_loop(&mut terminal, &cli, &mut state);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
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

/// Actions emitted by the update modal key handler.
enum UpdateModalAction {
    /// Keep the modal open or simply redraw the TUI.
    None,
    /// Exit the TUI.
    Quit,
    /// Download and install the selected update.
    Install(UpdateNotice),
}

/// Current state of the self-update workflow shown in the update modal.
enum UpdateInstallStatus {
    /// The user has not started installing the update.
    Idle,
    /// The installer is running in the background.
    Running(UpdateProgress),
    /// Replacement has been scheduled and the app should exit.
    Ready(UpdateInstall),
    /// The installer failed with a human-readable message.
    Failed(String),
}

/// Messages sent from the update worker to the UI thread.
enum UpdateInstallUpdate {
    /// Progress changed.
    Progress(UpdateProgress),
    /// The worker completed.
    Finished(Result<UpdateInstall, String>),
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

/// Lightweight size work kept by the preview thread after targets are sent to the UI.
struct PreviewSizeJob {
    index: usize,
    path: PathBuf,
    kind: RemovalKind,
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

#[derive(Default)]
/// Cached preview-list render output for the current width and target revision.
struct PreviewRenderCache {
    /// List width used to build the cached item strings.
    width: usize,
    /// Monotonic revision incremented whenever preview target text can change.
    revision: u64,
    /// Rendered list rows ready to convert into ratatui list items.
    items: Vec<RenderedPreviewItem>,
}

/// A rendered preview-list item optimized for the available width.
enum RenderedPreviewItem {
    /// Single-line representation used when the list has enough width.
    Single(String),
    /// Two-line representation used on narrow terminal widths.
    Compact {
        /// Path and kind label.
        label: String,
        /// Size text or pending-size placeholder.
        size: String,
    },
}

impl RenderedPreviewItem {
    /// Converts cached preview text into a ratatui list item.
    fn to_list_item(&self) -> ListItem<'_> {
        match self {
            Self::Single(line) => ListItem::new(line.as_str()),
            Self::Compact { label, size } => {
                ListItem::new(vec![Line::from(label.as_str()), Line::from(size.as_str())])
            }
        }
    }
}

/// Mutable state for the entire interactive session.
struct TuiState {
    /// Active screen or modal-like interaction mode.
    mode: TuiMode,
    /// Directory currently shown in browse mode.
    current_dir: PathBuf,
    /// Last directory used before returning from cleanup output.
    last_dir: Option<PathBuf>,
    /// Cached browse actions for `current_dir`.
    browser_actions: Vec<BrowserAction>,
    /// Directory that produced the current browse action cache.
    browser_actions_dir: Option<PathBuf>,
    /// Selected index in the browse action list.
    browser_index: usize,
    /// Available root shortcuts shown in the root selector.
    roots: Vec<BrowserRoot>,
    /// Selected index in the root selector.
    root_index: usize,
    /// Path text typed into the quick-switch modal.
    quick_path_input: String,
    /// Last quick-switch validation error.
    quick_path_error: Option<String>,
    /// Cleanup targets discovered for the current preview.
    preview_targets: Vec<RemovalTarget>,
    /// Cached preview-list rows for the current terminal width.
    preview_render_cache: PreviewRenderCache,
    /// Revision used to invalidate the preview render cache.
    preview_render_revision: u64,
    /// Selected index in the preview target list.
    preview_index: usize,
    /// Total size of preview targets whose sizes have been computed.
    preview_total_size: u64,
    /// Time spent discovering preview targets.
    preview_found_elapsed: std::time::Duration,
    /// Total elapsed time for preview discovery and sizing.
    preview_scan_elapsed: std::time::Duration,
    /// Current preview worker status.
    preview_status: PreviewStatus,
    /// Receiver for background preview scan updates.
    preview_scan_rx: Option<Receiver<PreviewScanUpdate>>,
    /// Monotonic id used to ignore stale preview worker messages.
    preview_scan_id: u64,
    /// Cached content counts for selected directory-like targets.
    selected_stats_cache: HashMap<PathBuf, TargetContentStats>,
    /// Receiver for selected-target stats updates.
    selected_stats_rx: Option<Receiver<SelectedStatsUpdate>>,
    /// Monotonic id used to ignore stale selected-stats messages.
    selected_stats_request_id: u64,
    /// Path currently being counted by the selected-stats worker.
    selected_stats_path: Option<PathBuf>,
    /// Pending update notice shown as an overlay modal.
    update_notice: Option<UpdateNotice>,
    /// Current self-update install state.
    update_install_status: UpdateInstallStatus,
    /// Receiver for background self-update progress.
    update_install_rx: Option<Receiver<UpdateInstallUpdate>>,
    /// Whether the terminal needs to be redrawn.
    needs_redraw: bool,
}

impl TuiState {
    /// Creates a new TUI state with the provided initial directory.
    fn new(initial_dir: Option<PathBuf>, update_notice: Option<UpdateNotice>) -> Self {
        let current_dir = initial_dir.unwrap_or_else(|| PathBuf::from("/"));
        Self {
            mode: TuiMode::Browse,
            current_dir,
            last_dir: None,
            browser_actions: Vec::new(),
            browser_actions_dir: None,
            browser_index: 0,
            roots: Vec::new(),
            root_index: 0,
            quick_path_input: String::new(),
            quick_path_error: None,
            preview_targets: Vec::new(),
            preview_render_cache: PreviewRenderCache::default(),
            preview_render_revision: 0,
            preview_index: 0,
            preview_total_size: 0,
            preview_found_elapsed: std::time::Duration::from_secs(0),
            preview_scan_elapsed: std::time::Duration::from_secs(0),
            preview_status: PreviewStatus::Ready,
            preview_scan_rx: None,
            preview_scan_id: 0,
            selected_stats_cache: HashMap::new(),
            selected_stats_rx: None,
            selected_stats_request_id: 0,
            selected_stats_path: None,
            update_notice,
            update_install_status: UpdateInstallStatus::Idle,
            update_install_rx: None,
            needs_redraw: true,
        }
    }

    fn mark_dirty(&mut self) {
        self.needs_redraw = true;
    }

    fn invalidate_browser_actions(&mut self) {
        self.browser_actions_dir = None;
    }

    fn invalidate_preview_render_cache(&mut self) {
        self.preview_render_revision = self.preview_render_revision.wrapping_add(1);
    }
}

/// Runs the TUI event loop until the user exits.
fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    cli: &Cli,
    state: &mut TuiState,
) -> Result<(), Box<dyn Error>> {
    state.roots = browser_roots(state.last_dir.as_deref());
    state.mark_dirty();
    let mut last_draw = Instant::now()
        .checked_sub(Duration::from_millis(100))
        .unwrap_or_else(Instant::now);

    loop {
        if process_preview_scan(state) {
            state.mark_dirty();
        }
        if process_selected_stats(state) {
            state.mark_dirty();
        }
        if process_update_install(state) {
            state.mark_dirty();
        }
        request_selected_stats_if_needed(state);
        refresh_browser_actions(state)?;
        let preview_animating = should_animate_preview(state);
        let update_animating = should_animate_update(state);
        let tick = Duration::from_millis(if preview_animating || update_animating {
            100
        } else {
            250
        });
        let should_draw = state.needs_redraw
            || (preview_animating && last_draw.elapsed() >= Duration::from_millis(100));
        let should_draw =
            should_draw || (update_animating && last_draw.elapsed() >= Duration::from_millis(100));

        if should_draw {
            if matches!(state.mode, TuiMode::Preview) {
                let width = preview_list_width(terminal.size()?.into());
                prepare_preview_render_cache(state, width);
            }
            terminal.draw(|frame| draw_tui(frame, state))?;
            state.needs_redraw = false;
            last_draw = Instant::now();
        }

        if !event::poll(tick)? {
            continue;
        }

        match event::read()? {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                if state.update_notice.is_some() {
                    match handle_update_modal_key(key.code, state) {
                        UpdateModalAction::None => {}
                        UpdateModalAction::Quit => return Ok(()),
                        UpdateModalAction::Install(notice) => {
                            start_update_install(notice, state);
                        }
                    }
                    continue;
                }

                match state.mode {
                    TuiMode::Browse => {
                        if handle_browse_key(key, state, cli)? {
                            return Ok(());
                        }
                    }
                    TuiMode::RootSelect => {
                        if handle_root_key(key, state)? {
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
            Event::Paste(text)
                if state.update_notice.is_none() && matches!(state.mode, TuiMode::RootSelect) =>
            {
                paste_quick_path_text(&text, state);
            }
            _ => {}
        }
    }
}

/// Returns whether the preview screen needs periodic redraws for elapsed-time text.
fn should_animate_preview(state: &TuiState) -> bool {
    matches!(state.mode, TuiMode::Preview)
        && matches!(
            state.preview_status,
            PreviewStatus::Scanning { .. } | PreviewStatus::CalculatingSize { .. }
        )
}

/// Returns whether the update modal needs periodic redraws.
fn should_animate_update(state: &TuiState) -> bool {
    state.update_notice.is_some()
        && matches!(state.update_install_status, UpdateInstallStatus::Running(_))
}

/// Pulls pending self-update worker messages into UI state.
fn process_update_install(state: &mut TuiState) -> bool {
    let Some(rx) = state.update_install_rx.take() else {
        return false;
    };

    let mut changed = false;
    let mut keep_rx = true;
    loop {
        match rx.try_recv() {
            Ok(UpdateInstallUpdate::Progress(progress)) => {
                state.update_install_status = UpdateInstallStatus::Running(progress);
                changed = true;
            }
            Ok(UpdateInstallUpdate::Finished(result)) => {
                state.update_install_status = match result {
                    Ok(install) => UpdateInstallStatus::Ready(install),
                    Err(message) => UpdateInstallStatus::Failed(message),
                };
                keep_rx = false;
                changed = true;
                break;
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                state.update_install_status =
                    UpdateInstallStatus::Failed("Update worker stopped unexpectedly".to_string());
                keep_rx = false;
                changed = true;
                break;
            }
        }
    }

    if keep_rx {
        state.update_install_rx = Some(rx);
    }
    changed
}

/// Starts the self-update worker and keeps the modal visible for progress.
fn start_update_install(notice: UpdateNotice, state: &mut TuiState) {
    if matches!(state.update_install_status, UpdateInstallStatus::Running(_)) {
        return;
    }

    let (tx, rx) = mpsc::channel();
    state.update_install_status = UpdateInstallStatus::Running(UpdateProgress::Preparing);
    state.update_install_rx = Some(rx);
    state.mark_dirty();

    thread::spawn(move || {
        let progress_tx = tx.clone();
        let result = update::install_update_with_progress(&notice, move |progress| {
            let _ = progress_tx.send(UpdateInstallUpdate::Progress(progress));
        })
        .map_err(|error| error.to_string());
        let _ = tx.send(UpdateInstallUpdate::Finished(result));
    });
}

/// Changes the current browse directory and invalidates cached directory entries.
fn set_current_dir(state: &mut TuiState, path: PathBuf) {
    if state.current_dir != path {
        state.current_dir = path;
        state.invalidate_browser_actions();
    }
    state.browser_index = 0;
    state.mark_dirty();
}

/// Opens the root-selection screen and refreshes its root list.
fn open_root_selector(state: &mut TuiState) {
    state.roots = browser_roots(Some(&state.current_dir));
    state.root_index = 0;
    state.quick_path_input.clear();
    state.quick_path_error = None;
    state.mode = TuiMode::RootSelect;
    state.mark_dirty();
}

/// Handles key input while the update notice modal is visible.
fn handle_update_modal_key(key: KeyCode, state: &mut TuiState) -> UpdateModalAction {
    if matches!(state.update_install_status, UpdateInstallStatus::Ready(_)) {
        return match key {
            KeyCode::Enter | KeyCode::Char('q') => UpdateModalAction::Quit,
            _ => UpdateModalAction::None,
        };
    }
    if matches!(state.update_install_status, UpdateInstallStatus::Running(_)) {
        return match key {
            KeyCode::Char('q') => UpdateModalAction::Quit,
            _ => UpdateModalAction::None,
        };
    }

    match key {
        KeyCode::Enter => {
            if let Some(notice) = state.update_notice.as_ref() {
                let _ = open_release_url(&notice.release_url);
            }
            state.update_notice = None;
            state.mark_dirty();
        }
        KeyCode::Char('u') => {
            if let Some(notice) = state.update_notice.as_ref().cloned() {
                state.mark_dirty();
                return UpdateModalAction::Install(notice);
            }
        }
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('d') => {
            state.update_notice = None;
            state.mark_dirty();
        }
        KeyCode::Char('q') => return UpdateModalAction::Quit,
        _ => {}
    }
    UpdateModalAction::None
}

/// Handles key input while browsing directories.
fn handle_browse_key(
    key: KeyEvent,
    state: &mut TuiState,
    cli: &Cli,
) -> Result<bool, Box<dyn Error>> {
    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Down | KeyCode::Char('j') if !state.browser_actions.is_empty() => {
            state.browser_index = (state.browser_index + 1) % state.browser_actions.len();
            state.mark_dirty();
        }
        KeyCode::Up | KeyCode::Char('k') if !state.browser_actions.is_empty() => {
            state.browser_index = if state.browser_index == 0 {
                state.browser_actions.len() - 1
            } else {
                state.browser_index - 1
            };
            state.mark_dirty();
        }
        KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
            if let Some(parent) = state.current_dir.parent() {
                set_current_dir(state, parent.to_path_buf());
            }
        }
        KeyCode::Char('r') => open_root_selector(state),
        _ if is_browse_activate_key(key) => match state.browser_actions.get(state.browser_index) {
            Some(BrowserAction::UseCurrent) => build_preview(state, cli)?,
            Some(BrowserAction::GoUp) => {
                if let Some(parent) = state.current_dir.parent() {
                    set_current_dir(state, parent.to_path_buf());
                }
            }
            Some(BrowserAction::ChangeRoot) => open_root_selector(state),
            Some(BrowserAction::Enter(path)) => set_current_dir(state, path.clone()),
            Some(BrowserAction::Quit) => return Ok(true),
            None => {}
        },
        _ => {}
    }
    Ok(false)
}

/// Returns whether a key should activate the selected browse item.
fn is_browse_activate_key(key: KeyEvent) -> bool {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return false;
    }

    matches!(
        key.code,
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l')
    )
}

/// Handles key input while selecting a root directory.
fn handle_root_key(key: KeyEvent, state: &mut TuiState) -> Result<bool, Box<dyn Error>> {
    if is_quick_path_paste_key(key) {
        paste_quick_path_from_clipboard(state);
        return Ok(false);
    }

    match key.code {
        KeyCode::Esc | KeyCode::Left => {
            state.mode = TuiMode::Browse;
            state.mark_dirty();
        }
        KeyCode::Backspace => {
            state.quick_path_input.pop();
            state.quick_path_error = None;
            state.mark_dirty();
        }
        KeyCode::Down if !state.roots.is_empty() => {
            state.root_index = (state.root_index + 1) % state.roots.len();
            state.mark_dirty();
        }
        KeyCode::Up if !state.roots.is_empty() => {
            state.root_index = if state.root_index == 0 {
                state.roots.len() - 1
            } else {
                state.root_index - 1
            };
            state.mark_dirty();
        }
        KeyCode::Enter => {
            if !state.quick_path_input.trim().is_empty() {
                submit_quick_path(state);
            } else if let Some(root) = state.roots.get(state.root_index) {
                set_current_dir(state, root.path.clone());
                state.mode = TuiMode::Browse;
                state.mark_dirty();
            }
        }
        KeyCode::Char(ch) if allows_text_input(key.modifiers) => {
            state.quick_path_input.push(ch);
            state.quick_path_error = None;
            state.mark_dirty();
        }
        _ => {}
    }
    Ok(false)
}

/// Returns whether a key should read the system clipboard into quick path.
fn is_quick_path_paste_key(key: KeyEvent) -> bool {
    matches!(
        key,
        KeyEvent {
            code: KeyCode::Char('v') | KeyCode::Char('V'),
            modifiers,
            ..
        } if modifiers.contains(KeyModifiers::CONTROL)
    ) || matches!(
        key,
        KeyEvent {
            code: KeyCode::Insert,
            modifiers,
            ..
        } if modifiers.contains(KeyModifiers::SHIFT)
    )
}

/// Returns whether a key modifier set represents normal text entry.
fn allows_text_input(modifiers: KeyModifiers) -> bool {
    !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// Applies a path typed into the quick-switch modal.
fn submit_quick_path(state: &mut TuiState) {
    let input = state.quick_path_input.trim().trim_matches('"');
    match resolve_quick_path(input, &state.current_dir) {
        Ok(candidate) => {
            state.mode = TuiMode::Browse;
            state.quick_path_input.clear();
            state.quick_path_error = None;
            set_current_dir(state, candidate);
        }
        Err(message) => {
            state.quick_path_error = Some(message);
            state.mark_dirty();
        }
    }
}

/// Appends pasted text to the quick-switch path input.
fn paste_quick_path_text(text: &str, state: &mut TuiState) {
    state
        .quick_path_input
        .push_str(text.trim().trim_matches('"'));
    state.quick_path_error = None;
    state.mark_dirty();
}

/// Reads the system clipboard and appends its text to the quick-switch path input.
fn paste_quick_path_from_clipboard(state: &mut TuiState) {
    match Clipboard::new().and_then(|mut clipboard| clipboard.get_text()) {
        Ok(text) if !text.trim().is_empty() => paste_quick_path_text(&text, state),
        Ok(_) => {
            state.quick_path_error = Some("Clipboard does not contain a path.".to_string());
            state.mark_dirty();
        }
        Err(err) => {
            state.quick_path_error = Some(format!("Clipboard is unavailable: {err}"));
            state.mark_dirty();
        }
    }
}

/// Resolves quick-switch input into an existing canonical directory path.
fn resolve_quick_path(input: &str, current_dir: &Path) -> Result<PathBuf, String> {
    if input.is_empty() {
        return Err("Enter a directory path.".to_string());
    }

    let raw_path = PathBuf::from(input);
    let candidate = if raw_path.is_absolute() {
        raw_path
    } else {
        current_dir.join(raw_path)
    };

    let canonical = candidate
        .canonicalize()
        .map_err(|err| format!("Cannot resolve path: {} ({err})", display_path(&candidate)))?;
    if !canonical.is_dir() {
        return Err(format!(
            "Path is not a directory: {}",
            display_path(&canonical)
        ));
    }

    Ok(canonical)
}

/// Handles key input while reviewing the preview list.
fn handle_preview_key(key: KeyCode, state: &mut TuiState) -> Result<PreviewAction, Box<dyn Error>> {
    match key {
        KeyCode::Char('q') => return Ok(PreviewAction::Quit),
        KeyCode::Esc | KeyCode::Char('b') => return Ok(PreviewAction::Back),
        KeyCode::Char('c')
            if matches!(state.preview_status, PreviewStatus::Ready)
                && !state.preview_targets.is_empty() =>
        {
            return Ok(PreviewAction::Clean);
        }
        KeyCode::Down | KeyCode::Char('j') if !state.preview_targets.is_empty() => {
            state.preview_index = (state.preview_index + 1) % state.preview_targets.len();
            state.mark_dirty();
        }
        KeyCode::Up | KeyCode::Char('k') if !state.preview_targets.is_empty() => {
            state.preview_index = if state.preview_index == 0 {
                state.preview_targets.len() - 1
            } else {
                state.preview_index - 1
            };
            state.mark_dirty();
        }
        _ => {}
    }
    Ok(PreviewAction::None)
}

/// Opens a release URL using the platform's default browser.
fn open_release_url(url: &str) -> io::Result<()> {
    #[cfg(windows)]
    {
        Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
            .map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(url).spawn().map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open").arg(url).spawn().map(|_| ())
    }
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
    state.selected_stats_cache.clear();
    state.selected_stats_rx = None;
    state.selected_stats_path = None;
    state.invalidate_preview_render_cache();
    state.preview_status = PreviewStatus::Scanning {
        started_at: Instant::now(),
    };
    state.mode = TuiMode::Preview;
    state.mark_dirty();

    thread::spawn(move || {
        let scan_start = Instant::now();
        let targets = collect_cleanup_targets_fast(&path, &config);
        let size_jobs: Vec<_> = targets
            .iter()
            .enumerate()
            .map(|(index, target)| PreviewSizeJob {
                index,
                path: target.path().to_path_buf(),
                kind: target.kind(),
            })
            .collect();

        if tx
            .send(PreviewScanUpdate::TargetsFound {
                scan_id,
                targets,
                scan_elapsed: scan_start.elapsed(),
            })
            .is_err()
        {
            return;
        }

        let mut running_total_size = 0u64;
        let mut batch = Vec::new();
        let batch_size = 16usize;
        let job_count = size_jobs.len();

        for (position, job) in size_jobs.into_iter().enumerate() {
            let size = compute_target_size_for_path(&job.path, job.kind);
            running_total_size += size;
            batch.push((job.index, size));

            let is_last = position + 1 == job_count;
            if batch.len() >= batch_size || is_last {
                let sized_count = position + 1;
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
fn process_preview_scan(state: &mut TuiState) -> bool {
    let mut disconnected = false;
    let mut updates = Vec::new();
    let mut changed = false;

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
                state.invalidate_preview_render_cache();
                if state.preview_targets.is_empty() {
                    state.preview_status = PreviewStatus::Ready;
                    state.preview_scan_rx = None;
                } else {
                    state.preview_status = PreviewStatus::CalculatingSize {
                        started_at: Instant::now(),
                        target_count: state.preview_targets.len(),
                        sized_count: 0,
                    };
                }
                changed = true;
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
                state.invalidate_preview_render_cache();
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
                changed = true;
            }
            _ => {}
        }
    }

    if disconnected && !matches!(state.preview_status, PreviewStatus::Ready) {
        state.preview_scan_rx = None;
        state.preview_status = PreviewStatus::Failed("Scan stopped unexpectedly.".to_string());
        changed = true;
    }

    changed
}

/// Pulls pending selected-target content counts into UI state.
fn process_selected_stats(state: &mut TuiState) -> bool {
    let mut updates = Vec::new();
    let mut changed = false;
    let mut disconnected = false;

    if let Some(rx) = state.selected_stats_rx.as_ref() {
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
            SelectedStatsUpdate::Ready {
                request_id,
                path,
                stats,
            } if request_id == state.selected_stats_request_id => {
                let is_selected = state.selected_stats_path.as_ref() == Some(&path);
                state.selected_stats_cache.insert(path, stats);
                if is_selected {
                    state.selected_stats_rx = None;
                }
                changed = true;
            }
            _ => {}
        }
    }

    if disconnected {
        state.selected_stats_rx = None;
    }

    changed
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
    let kind = entry.kind();
    let path_clone = path.clone();
    let (tx, rx) = mpsc::channel();

    state.selected_stats_request_id = request_id;
    state.selected_stats_path = Some(path);
    state.selected_stats_rx = Some(rx);

    thread::spawn(move || {
        let stats = summarize_target_contents_for_path(&path_clone, kind);
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
    state.invalidate_preview_render_cache();
    state.preview_render_cache.items.clear();
    state.preview_index = 0;
    state.preview_total_size = 0;
    state.preview_found_elapsed = std::time::Duration::from_secs(0);
    state.preview_scan_elapsed = std::time::Duration::from_secs(0);
    state.selected_stats_cache.clear();
    state.selected_stats_rx = None;
    state.selected_stats_path = None;
    state.preview_status = PreviewStatus::Ready;
    state.mode = TuiMode::Browse;
    state.mark_dirty();
}

/// Rebuilds the browse-list actions for the current directory.
fn refresh_browser_actions(state: &mut TuiState) -> Result<(), Box<dyn Error>> {
    if !matches!(state.mode, TuiMode::Browse) {
        return Ok(());
    }

    if state.browser_actions_dir.as_ref() == Some(&state.current_dir) {
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
    state.browser_actions_dir = Some(state.current_dir.clone());
    if state.browser_index >= state.browser_actions.len() {
        state.browser_index = state.browser_actions.len().saturating_sub(1);
    }
    state.mark_dirty();
    Ok(())
}

/// Draws the active TUI screen for the current mode.
fn draw_tui(frame: &mut Frame, state: &TuiState) {
    match state.mode {
        TuiMode::Browse => draw_browse_view(frame, state),
        TuiMode::RootSelect => draw_root_view(frame, state),
        TuiMode::Preview => draw_preview_view(frame, state),
    }

    if let Some(notice) = state.update_notice.as_ref() {
        render_update_modal(frame, notice, &state.update_install_status);
    }
}

/// Draws the main directory-browsing screen.
fn draw_browse_view(frame: &mut Frame, state: &TuiState) {
    let shell = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(frame.area());
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(shell[1]);

    render_header(
        frame,
        shell[0],
        "dust",
        "Browse",
        &state.current_dir,
        "Ready",
    );

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
        "Enter: preview   r: switch folder   Backspace/←: parent   q: quit",
    );
}

/// Draws the modal root-selection screen on top of the browse view.
fn draw_root_view(frame: &mut Frame, state: &TuiState) {
    draw_browse_view(frame, state);
    let area = centered_rect(92, 70, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block("Switch Folder"), area);

    let inner = area.inner(Margin {
        vertical: 1,
        horizontal: 2,
    });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(inner);

    let input_text = if state.quick_path_input.is_empty() {
        "<type or paste a directory path>".to_string()
    } else {
        state.quick_path_input.clone()
    };
    let input_style = if state.quick_path_input.is_empty() {
        Style::default().fg(COLOR_MUTED)
    } else {
        Style::default().fg(COLOR_TEXT_SOFT)
    };
    let mut input_lines = vec![
        Line::from(Span::styled(
            "Quick path",
            Style::default()
                .fg(COLOR_ACCENT_ALT)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(input_text, input_style)),
    ];
    if let Some(error) = state.quick_path_error.as_ref() {
        input_lines.push(Line::from(Span::styled(
            truncate_middle(error, inner_width(chunks[0]).saturating_sub(1)),
            Style::default().fg(COLOR_WARNING),
        )));
    } else {
        input_lines.push(Line::from("Choose a starting point below, or type a path."));
    }
    frame.render_widget(
        Paragraph::new(input_lines).wrap(Wrap { trim: true }),
        chunks[0],
    );

    let items: Vec<ListItem> = state
        .roots
        .iter()
        .map(|root| {
            ListItem::new(truncate_middle(
                &root.label,
                inner_width(chunks[1]).saturating_sub(2),
            ))
        })
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
        Span::raw(
            "type path   Ctrl+V/Shift+Insert: paste   ↑/↓: roots   Enter: switch   Esc: back",
        ),
    ]))
    .wrap(Wrap { trim: true });
    frame.render_widget(footer, chunks[2]);
}

/// Draws the update-available modal over the active TUI screen.
fn render_update_modal(frame: &mut Frame, notice: &UpdateNotice, status: &UpdateInstallStatus) {
    let area = centered_rect_fixed(92, 17, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block("Update available"), area);

    let inner = area.inner(Margin {
        vertical: 1,
        horizontal: 2,
    });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(2)])
        .split(inner);

    let mut lines = vec![
        Line::from(Span::styled(
            format!("dust v{} is available", notice.latest_version),
            Style::default()
                .fg(COLOR_TEXT_SOFT)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("Current version: v{}", notice.current_version)),
        Line::from(format!("Latest version:  v{}", notice.latest_version)),
        Line::from(""),
        Line::from(Span::styled(
            "Release",
            Style::default()
                .fg(COLOR_ACCENT_ALT)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(truncate_middle(
            &notice.release_url,
            inner_width(chunks[0]).saturating_sub(1),
        )),
        Line::from(""),
    ];
    lines.extend(update_status_lines(
        notice,
        status,
        inner_width(chunks[0]).saturating_sub(1),
    ));

    let details = Paragraph::new(lines).wrap(Wrap { trim: true });
    frame.render_widget(details, chunks[0]);

    let footer = Paragraph::new(update_footer_line(status)).wrap(Wrap { trim: true });
    frame.render_widget(footer, chunks[1]);
}

/// Builds status text for the update modal.
fn update_status_lines(
    notice: &UpdateNotice,
    status: &UpdateInstallStatus,
    max_width: usize,
) -> Vec<Line<'static>> {
    match status {
        UpdateInstallStatus::Idle => vec![Line::from(if notice.asset_download_url.is_some() {
            "Press u to download and install the matching archive for this platform."
        } else {
            "No matching archive was found for this platform; open the release page instead."
        })],
        UpdateInstallStatus::Running(progress) => vec![
            Line::from(Span::styled(
                "Installing",
                Style::default()
                    .fg(COLOR_SUCCESS)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(truncate_middle(&update_progress_text(progress), max_width)),
            Line::from(progress_bar_line(progress, max_width)),
        ],
        UpdateInstallStatus::Ready(install) => vec![
            Line::from(Span::styled(
                "Update ready",
                Style::default()
                    .fg(COLOR_SUCCESS)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("dust v{} is scheduled.", install.latest_version)),
            Line::from(truncate_middle(
                &format!("Target: {}", install.target_exe.display()),
                max_width,
            )),
            Line::from("Exit dust to finish replacing the current binary."),
        ],
        UpdateInstallStatus::Failed(message) => vec![
            Line::from(Span::styled(
                "Update failed",
                Style::default()
                    .fg(COLOR_WARNING)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(truncate_middle(message, max_width)),
            Line::from("Open the release page to download the archive manually."),
        ],
    }
}

/// Builds the update modal footer for the current update state.
fn update_footer_line(status: &UpdateInstallStatus) -> Line<'static> {
    match status {
        UpdateInstallStatus::Running(_) => Line::from(vec![
            Span::styled("q", Style::default().fg(COLOR_WARNING)),
            Span::raw(": quit"),
        ]),
        UpdateInstallStatus::Ready(_) => Line::from(vec![
            Span::styled("Enter", Style::default().fg(COLOR_SUCCESS)),
            Span::raw(": exit   "),
            Span::styled("q", Style::default().fg(COLOR_WARNING)),
            Span::raw(": quit"),
        ]),
        UpdateInstallStatus::Failed(_) | UpdateInstallStatus::Idle => Line::from(vec![
            Span::styled("u", Style::default().fg(COLOR_SUCCESS)),
            Span::raw(": update   "),
            Span::styled("Enter", Style::default().fg(COLOR_SUCCESS)),
            Span::raw(": open   "),
            Span::styled("Esc", Style::default().fg(COLOR_WARNING)),
            Span::raw(": dismiss   "),
            Span::styled("n", Style::default().fg(COLOR_WARNING)),
            Span::raw(": remind later   "),
            Span::styled("q", Style::default().fg(COLOR_WARNING)),
            Span::raw(": quit"),
        ]),
    }
}

/// Formats the current update progress.
fn update_progress_text(progress: &UpdateProgress) -> String {
    match progress {
        UpdateProgress::Preparing => "Preparing update...".to_string(),
        UpdateProgress::Downloading { downloaded, total } => {
            if let Some(total) = total {
                format!(
                    "Downloading {} / {}",
                    format_size(*downloaded),
                    format_size(*total)
                )
            } else {
                format!("Downloading {}", format_size(*downloaded))
            }
        }
        UpdateProgress::Extracting => "Extracting archive...".to_string(),
        UpdateProgress::Scheduling => "Scheduling binary replacement...".to_string(),
    }
}

/// Builds a compact textual progress bar for determinate download progress.
fn progress_bar_line(progress: &UpdateProgress, max_width: usize) -> String {
    let width = max_width.clamp(10, 40);
    let (filled, percent) = match progress {
        UpdateProgress::Downloading {
            downloaded,
            total: Some(total),
        } if *total > 0 => {
            let filled = ((*downloaded).min(*total) * width as u64 / *total) as usize;
            let percent = ((*downloaded).min(*total) * 100 / *total) as usize;
            (filled, Some(percent))
        }
        _ => (0, None),
    };
    let empty = width.saturating_sub(filled);
    if let Some(percent) = percent {
        format!("[{}{}] {percent}%", "#".repeat(filled), ".".repeat(empty))
    } else {
        format!("[{}]", ".".repeat(width))
    }
}

/// Draws the cleanup preview screen with targets and details.
fn draw_preview_view(frame: &mut Frame, state: &TuiState) {
    let shell = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
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

    let mut list_state = ListState::default();
    if !state.preview_targets.is_empty() {
        list_state.select(Some(state.preview_index));
    }
    let items: Vec<ListItem> = state
        .preview_render_cache
        .items
        .iter()
        .map(RenderedPreviewItem::to_list_item)
        .collect();
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
        Span::styled(
            app,
            Style::default()
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(COLOR_MUTED),
        ),
        Span::raw("  "),
        Span::styled(
            mode,
            Style::default()
                .fg(COLOR_ACCENT_ALT)
                .add_modifier(Modifier::DIM),
        ),
        Span::raw("   "),
        Span::styled("•", Style::default().fg(COLOR_MUTED)),
        Span::raw("  "),
        Span::raw(truncate_middle(
            &display_path(path),
            inner_width(chunks[1]).saturating_sub(42),
        )),
        Span::raw("  "),
        Span::styled("•", Style::default().fg(COLOR_MUTED)),
        Span::raw("  "),
        Span::styled("Status", Style::default().fg(COLOR_SUCCESS)),
        Span::raw(" "),
        Span::raw(truncate_middle(
            status,
            inner_width(chunks[1]).saturating_sub(56),
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
            Style::default()
                .fg(COLOR_ACCENT_ALT)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("Version: v{}", env!("CARGO_PKG_VERSION"))),
        Line::from(format!("Folder: {}", file_name_or_path(&state.current_dir))),
        Line::from(format!(
            "Entries: {}",
            state.browser_actions.len().saturating_sub(4)
        )),
        Line::from(format!(
            "Parent folder: {}",
            if state.current_dir.parent().is_some() {
                "available"
            } else {
                "none"
            }
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Keys",
            Style::default()
                .fg(COLOR_WARNING)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("Use arrows or j/k to move."),
        Line::from("Press Enter to open a folder."),
        Line::from("Choose [Scan here] to preview this folder."),
        Line::from("Press r to switch folders quickly."),
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
                Style::default()
                    .fg(COLOR_ACCENT_ALT)
                    .add_modifier(Modifier::BOLD),
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
        lines.extend(scan_summary_lines(
            state,
            inner_width(area).saturating_sub(1),
        ));
        lines.extend([
            Line::from(Span::styled(
                "Keys",
                Style::default()
                    .fg(COLOR_WARNING)
                    .add_modifier(Modifier::BOLD),
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
                Style::default()
                    .fg(COLOR_ACCENT_ALT)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("Type: -"),
            Line::from("Size: -"),
            Line::from("Nothing to remove yet."),
            Line::from(""),
        ];
        lines.extend(scan_summary_lines(
            state,
            inner_width(area).saturating_sub(1),
        ));
        lines.extend([
            Line::from(Span::styled(
                "Keys",
                Style::default()
                    .fg(COLOR_WARNING)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("Review the planned targets."),
            Line::from("Use ↑/↓ or j/k to inspect each entry."),
            Line::from("Press c to remove these items."),
            Line::from("Press Esc or b to go back."),
        ]);
        lines
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block("Info"))
            .wrap(Wrap { trim: true }),
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
            Style::default()
                .fg(COLOR_ACCENT_ALT)
                .add_modifier(Modifier::BOLD),
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
        PreviewStatus::Scanning { started_at } => {
            format!("scanning • {:.1?}", started_at.elapsed())
        }
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
        Style::default()
            .fg(COLOR_SUCCESS)
            .add_modifier(Modifier::BOLD),
    ))];

    match &state.preview_status {
        PreviewStatus::Scanning { started_at } => {
            lines.push(Line::from(truncate_middle(
                &format!("Looking for items… {:.1?}", started_at.elapsed()),
                max_width,
            )));
            lines.push(Line::from(
                "The list will appear as soon as scanning finishes.",
            ));
        }
        PreviewStatus::CalculatingSize {
            started_at,
            target_count,
            sized_count,
        } => {
            lines.push(Line::from(truncate_middle(
                &format!(
                    "Found {target_count} item(s) in {:.1?}",
                    state.preview_found_elapsed
                ),
                max_width,
            )));
            lines.push(Line::from(truncate_middle(
                &format!(
                    "Sized {sized_count}/{target_count}… {:.1?}",
                    started_at.elapsed()
                ),
                max_width,
            )));
        }
        PreviewStatus::Ready => {
            lines.push(Line::from(truncate_middle(
                &format!(
                    "Found {} item(s) in {:.1?}",
                    state.preview_targets.len(),
                    state.preview_found_elapsed
                ),
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
        BrowserAction::UseCurrent => truncate_middle(
            &format!("[Scan here] {}", display_path(current_dir)),
            max_width,
        ),
        BrowserAction::GoUp => "[Up] ..".to_string(),
        BrowserAction::ChangeRoot => "[Roots]".to_string(),
        BrowserAction::Enter(path) => {
            truncate_middle(&format!("{}/", file_name_or_path(path)), max_width)
        }
        BrowserAction::Quit => "[Quit]".to_string(),
    }
}

/// Calculates the usable preview-list width for a full terminal area.
fn preview_list_width(area: Rect) -> usize {
    let shell = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(area);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(shell[1]);
    let list_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(7)])
        .split(body[0]);

    inner_width(list_chunks[1]).saturating_sub(4)
}

/// Rebuilds cached preview-list text when width or target content changes.
fn prepare_preview_render_cache(state: &mut TuiState, max_width: usize) {
    if state.preview_render_cache.width == max_width
        && state.preview_render_cache.revision == state.preview_render_revision
    {
        return;
    }

    state.preview_render_cache.width = max_width;
    state.preview_render_cache.revision = state.preview_render_revision;
    state.preview_render_cache.items = state
        .preview_targets
        .iter()
        .map(|entry| render_preview_item(entry, max_width))
        .collect();
}

/// Renders a target into either a single-line or compact preview item.
fn render_preview_item(entry: &RemovalTarget, max_width: usize) -> RenderedPreviewItem {
    if max_width <= 48 {
        return RenderedPreviewItem::Compact {
            label: truncate_middle(
                &format!("[{}] {}", entry.label(), display_path(entry.path())),
                max_width,
            ),
            size: truncate_middle(&format!("size: {}", target_size_text(entry)), max_width),
        };
    }

    RenderedPreviewItem::Single(format_preview_entry_single_line(entry, max_width))
}

/// Formats a preview entry as a single line with aligned size information.
fn format_preview_entry_single_line(entry: &RemovalTarget, max_width: usize) -> String {
    let size = format!("({})", target_size_text(entry));
    let prefix = format!("[{}] ", entry.label());
    if max_width <= prefix.len() + size.len() + 1 {
        return truncate_middle(
            &format!("{prefix}{}", display_path(entry.path())),
            max_width,
        );
    }
    let path_width = max_width.saturating_sub(prefix.len() + size.len() + 1);
    let path = truncate_middle(&display_path(entry.path()), path_width);
    format!("{prefix}{path} {size}")
}

/// Builds the list of root entries available to the browse UI.
fn browser_roots(last_dir: Option<&Path>) -> Vec<BrowserRoot> {
    let mut roots = Vec::new();
    if let Some(path) = last_dir {
        push_browser_root(
            &mut roots,
            BrowserRoot::new(format!("Last: {}", display_path(path)), path.to_path_buf()),
        );
    }
    if let Ok(current_dir) = env::current_dir() {
        push_browser_root(
            &mut roots,
            BrowserRoot::new(
                format!("Current: {}", display_path(&current_dir)),
                current_dir,
            ),
        );
    }
    if let Some(home_dir) = home_dir() {
        push_browser_root(
            &mut roots,
            BrowserRoot::new(format!("Home: {}", display_path(&home_dir)), home_dir),
        );
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
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|file_type| file_type.is_dir())
                .map(|_| entry.path())
        })
        .collect::<Vec<_>>();

    directories.sort_by_key(|entry| file_name_or_path(entry).to_ascii_lowercase());
    Ok(directories)
}

/// Returns the file name of a path, falling back to the full path string.
fn file_name_or_path(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| display_path(path))
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

/// Converts a filesystem path to a display string.
fn display_path(path: &Path) -> String {
    display_path_text(&path.display().to_string())
}

/// Removes platform-specific path decoration that is useful internally but noisy in the UI.
fn display_path_text(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{rest}")
    } else if let Some(rest) = path.strip_prefix("\\\\?\\") {
        rest.to_string()
    } else {
        path.to_string()
    }
}

/// Truncates text in the middle so both the beginning and end remain visible.
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

/// Builds a centered rectangle with fixed width and height caps.
fn centered_rect_fixed(width: u16, height: u16, area: Rect) -> Rect {
    let popup_width = width.min(area.width.saturating_sub(2)).max(1);
    let popup_height = height.min(area.height.saturating_sub(2)).max(1);
    let horizontal_margin = area.width.saturating_sub(popup_width) / 2;
    let vertical_margin = area.height.saturating_sub(popup_height) / 2;

    Rect {
        x: area.x + horizontal_margin,
        y: area.y + vertical_margin,
        width: popup_width,
        height: popup_height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{self, File},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn selected_stats_are_requested_for_directory_like_targets() {
        assert!(supports_selected_stats(RemovalKind::Directory));
        assert!(supports_selected_stats(RemovalKind::LogDirectory));
        assert!(!supports_selected_stats(RemovalKind::FileGroup));
    }

    #[test]
    fn quick_path_rejects_missing_paths() {
        let root = create_temp_dir("quick_path_rejects_missing_paths");
        let missing = resolve_quick_path("missing", &root);

        assert!(missing.is_err());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn quick_path_rejects_files() {
        let root = create_temp_dir("quick_path_rejects_files");
        let file_path = root.join("file.txt");
        File::create(&file_path).unwrap();

        let result = resolve_quick_path(&file_path.to_string_lossy(), &root);

        assert!(result.is_err());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn quick_path_resolves_relative_directories_from_current_dir() {
        let root = create_temp_dir("quick_path_resolves_relative_directories");
        let child = root.join("child");
        fs::create_dir_all(&child).unwrap();

        let result = resolve_quick_path("child", &root).unwrap();

        assert_eq!(result, child.canonicalize().unwrap());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn quick_path_paste_keeps_q_as_input_after_text_started() {
        let root = create_temp_dir("quick_path_paste_keeps_q_as_input");
        let mut state = TuiState::new(Some(root.clone()), None);
        state.mode = TuiMode::RootSelect;

        for ch in "keys Enter: preview   r: roots   Backspace/←: parent   q: quit".chars() {
            let should_quit = handle_root_key(key(KeyCode::Char(ch)), &mut state).unwrap();
            assert!(!should_quit, "pasted character {ch:?} should not quit");
        }

        assert!(state.quick_path_input.contains("q: quit"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn quick_path_treats_letters_as_path_input() {
        let root = create_temp_dir("quick_path_treats_letters_as_path_input");
        let mut state = TuiState::new(Some(root.clone()), None);
        state.mode = TuiMode::RootSelect;

        for ch in "D:\\Projects\\jkhbq".chars() {
            let should_quit = handle_root_key(key(KeyCode::Char(ch)), &mut state).unwrap();
            assert!(!should_quit);
        }

        assert_eq!(state.quick_path_input, "D:\\Projects\\jkhbq");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn quick_path_ctrl_q_does_not_quit() {
        let root = create_temp_dir("quick_path_ctrl_q_does_not_quit");
        let mut state = TuiState::new(Some(root.clone()), None);
        state.mode = TuiMode::RootSelect;

        let should_quit = handle_root_key(
            modified_key(KeyCode::Char('q'), KeyModifiers::CONTROL),
            &mut state,
        )
        .unwrap();

        assert!(!should_quit);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn quick_path_detects_clipboard_paste_keys() {
        assert!(is_quick_path_paste_key(modified_key(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL
        )));
        assert!(is_quick_path_paste_key(modified_key(
            KeyCode::Insert,
            KeyModifiers::SHIFT
        )));
        assert!(!is_quick_path_paste_key(key(KeyCode::Char('v'))));
    }

    #[test]
    fn browse_activation_ignores_control_modified_enter() {
        assert!(!is_browse_activate_key(modified_key(
            KeyCode::Enter,
            KeyModifiers::CONTROL
        )));
        assert!(is_browse_activate_key(key(KeyCode::Enter)));
    }

    #[test]
    fn quick_path_paste_preserves_long_paths_with_spaces() {
        let root = create_temp_dir("quick_path_paste_preserves_long_paths");
        let child = root.join("RFC-12020CMDB Portal - Migrate Background Jobs into CMDB-Portal");
        fs::create_dir_all(&child).unwrap();
        let mut state = TuiState::new(Some(root.clone()), None);
        state.mode = TuiMode::RootSelect;

        paste_quick_path_text(&format!("\"{}\"", child.display()), &mut state);
        submit_quick_path(&mut state);

        assert_eq!(state.current_dir, child.canonicalize().unwrap());
        assert!(matches!(state.mode, TuiMode::Browse));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn display_path_hides_windows_verbatim_prefix() {
        assert_eq!(
            display_path_text("\\\\?\\D:\\Projects\\Acer"),
            "D:\\Projects\\Acer"
        );
    }

    #[test]
    fn display_path_hides_windows_unc_verbatim_prefix() {
        assert_eq!(
            display_path_text("\\\\?\\UNC\\server\\share\\folder"),
            "\\\\server\\share\\folder"
        );
    }

    fn create_temp_dir(label: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("dust_interactive_{label}_{timestamp}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn key(code: KeyCode) -> KeyEvent {
        modified_key(code, KeyModifiers::empty())
    }

    fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }
}
