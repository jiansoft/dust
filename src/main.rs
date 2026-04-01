mod utils;

use clap::Parser;
use clap::ValueEnum;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use dialoguer::Confirm;
use indicatif::{ProgressBar, ProgressStyle};
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
use serde::Serialize;
use std::{
    env,
    error::Error,
    fs, io,
    io::Write,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::Instant,
};
use utils::{
    DeleteAction, DeleteOperation, RemovalTarget, ScanConfig, ScanMode, calculate_entries_size,
    collect_cleanup_targets, format_size,
};

/// 清除常見編譯暫存資料夾與產物檔案
#[derive(Parser)]
#[command(name = "dust")]
#[command(
    about = "刪除 bin/obj/node_modules/target/zig-cache 等編譯產物的小工具",
    long_about = None
)]
#[command(
    after_help = "Examples:\n  dust D:\\Project\\MyApp\n  dust . --dry-run\n  dust . --exclude '**/vendor/**' --exclude '**/third_party/**'\n  dust . --dirs-only\n  dust . --files-only\n  dust . --yes"
)]
struct Cli {
    /// 要掃描的根目錄
    path: Option<String>,

    /// 只列出符合項目，不實際刪除
    #[arg(long)]
    dry_run: bool,

    /// 略過刪除前確認
    #[arg(short = 'y', long)]
    yes: bool,

    /// 只清理資料夾
    #[arg(long, conflicts_with = "files_only")]
    dirs_only: bool,

    /// 只清理檔案
    #[arg(long, conflicts_with = "dirs_only")]
    files_only: bool,

    /// 以 glob 排除路徑，可重複使用
    #[arg(long, value_name = "GLOB")]
    exclude: Vec<String>,

    /// 隱藏一般輸出
    #[arg(long)]
    quiet: bool,

    /// 停用刪除進度列
    #[arg(long)]
    no_progress: bool,

    /// 進度列風格
    #[arg(long, value_enum, default_value_t = ProgressStyleKind::Soft)]
    progress_style: ProgressStyleKind,

    /// 輸出 JSON 結果
    #[arg(long)]
    json: bool,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ProgressStyleKind {
    Soft,
    Minimal,
}

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

fn main() -> Result<(), Box<dyn Error>> {
    run()
}

fn run() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    if cli.json && cli.path.is_none() {
        return Err(
            "`--json` requires a path argument and does not support interactive mode".into(),
        );
    }

    if cli.path.is_none() && !cli.json {
        return run_interactive(cli);
    }

    run_once(&cli, cli.path.clone().unwrap_or_default())?;
    Ok(())
}

fn run_interactive(cli: Cli) -> Result<(), Box<dyn Error>> {
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

fn run_once(cli: &Cli, folder: String) -> Result<(), Box<dyn Error>> {
    let mode = scan_mode_from_cli(&cli);
    let config = ScanConfig::new(&cli.exclude, mode)?;

    if cli.json && !cli.dry_run && !cli.yes {
        return Err("`--json` requires `--dry-run` or `--yes` to avoid interactive output".into());
    }

    if !Path::new(&folder).exists() {
        if cli.json {
            print_json(&JsonRun::missing_path(&folder))?;
        } else if !cli.quiet {
            eprintln!("Path not found: {}", folder);
        }
        return Ok(());
    }

    if !cli.json && !cli.quiet {
        println!("Scanning: {}", folder);
    }

    let scan_start = Instant::now();
    let to_delete = collect_cleanup_targets(Path::new(&folder), &config);
    let scan_elapsed = scan_start.elapsed();
    let total_size = calculate_entries_size(&to_delete);
    let step_count = build_delete_operations(&to_delete).len();

    if to_delete.is_empty() {
        if cli.json {
            print_json(&JsonRun::no_matches(
                &folder,
                mode,
                &cli.exclude,
                cli.dry_run,
                scan_elapsed,
                0,
            ))?;
        } else if !cli.quiet {
            println!("No matching files or directories found.");
        }
        return Ok(());
    }

    if !cli.json && !cli.quiet {
        print_plan(&to_delete, scan_elapsed);
    }

    if cli.dry_run {
        if cli.json {
            print_json(&JsonRun::preview(
                &folder,
                mode,
                &cli.exclude,
                scan_elapsed,
                &to_delete,
                total_size,
                step_count,
            ))?;
        } else if !cli.quiet {
            println!("Dry run complete. No changes were made.");
        }
        return Ok(());
    }

    if !confirm_deletion(cli.yes)? {
        if cli.json {
            print_json(&JsonRun::cancelled(
                &folder,
                mode,
                &cli.exclude,
                scan_elapsed,
                &to_delete,
                total_size,
                step_count,
            ))?;
        } else if !cli.quiet {
            println!("Operation cancelled.");
        }
        return Ok(());
    }

    let outcome = execute_removal(
        &to_delete,
        cli.json,
        cli.quiet,
        cli.no_progress,
        cli.progress_style,
    );
    if cli.json {
        print_json(&JsonRun::completed(
            &folder,
            mode,
            &cli.exclude,
            scan_elapsed,
            &to_delete,
            total_size,
            outcome.step_count,
            outcome,
        ))?;
    }
    Ok(())
}

fn scan_mode_from_cli(cli: &Cli) -> ScanMode {
    if cli.dirs_only {
        ScanMode::DirectoriesOnly
    } else if cli.files_only {
        ScanMode::FilesOnly
    } else {
        ScanMode::All
    }
}

fn print_plan(entries: &[utils::RemovalTarget], scan_elapsed: std::time::Duration) {
    println!(
        "Found {} item(s), total {} (scan time: {:.2?}):",
        entries.len(),
        format_size(calculate_entries_size(entries)),
        scan_elapsed
    );

    for entry in entries {
        println!(
            " - [{}] {} ({})",
            entry.label(),
            entry.path().display(),
            format_size(entry.size())
        );
    }
}

fn confirm_deletion(skip_confirmation: bool) -> Result<bool, Box<dyn Error>> {
    if skip_confirmation {
        return Ok(true);
    }

    Ok(Confirm::new()
        .with_prompt("Delete these items?")
        .default(false)
        .interact()?)
}

fn execute_removal(
    entries: &[utils::RemovalTarget],
    json_mode: bool,
    quiet: bool,
    no_progress: bool,
    progress_style: ProgressStyleKind,
) -> RemovalOutcome {
    let start_time = Instant::now();
    let mut success = 0usize;
    let mut failed = 0usize;
    let mut failures = Vec::new();
    let operations = build_delete_operations(entries);

    let show_progress = !json_mode && !quiet && !no_progress;
    let progress = show_progress.then(|| {
        let bar = ProgressBar::new(operations.len() as u64);
        let style = progress_bar_style(progress_style);
        bar.set_style(style);
        bar.set_message(progress_message("Preparing delete plan", progress_style));
        bar
    });

    for operation in &operations {
        if let Some(bar) = &progress {
            bar.set_message(format_progress_message(
                &operation.1,
                &operation.2,
                progress_style,
            ));
        }

        if let Err(err) = execute_operation(operation) {
            failed += 1;
            failures.push(FailedRemoval {
                path: operation.2.path().display().to_string(),
                kind: operation.0,
                error: err.to_string(),
            });
        } else {
            success += 1;
        }

        if let Some(bar) = &progress {
            bar.inc(1);
        }
    }

    if !json_mode && !quiet {
        if let Some(bar) = progress {
            bar.finish_and_clear();
        }
        println!(
            "Completed: {} step(s) succeeded, {} failed, across {} target(s).",
            success,
            failed,
            entries.len()
        );
        println!("Elapsed: {:.2?}", start_time.elapsed());
    }

    RemovalOutcome {
        deleted: success,
        failed,
        step_count: operations.len(),
        elapsed_ms: start_time.elapsed().as_millis(),
        failures,
    }
}

fn print_json(payload: &JsonRun) -> Result<(), Box<dyn Error>> {
    println!("{}", serde_json::to_string_pretty(payload)?);
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
struct JsonRun {
    status: JsonStatus,
    root: String,
    dry_run: bool,
    mode: &'static str,
    exclude: Vec<String>,
    scan_elapsed_ms: u128,
    total_size_bytes: u64,
    target_count: usize,
    step_count: usize,
    targets: Vec<JsonTarget>,
    removal: Option<RemovalOutcome>,
    message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum JsonStatus {
    MissingPath,
    NoMatches,
    Preview,
    Cancelled,
    Completed,
}

#[derive(Debug, Serialize)]
struct JsonTarget {
    path: String,
    kind: utils::RemovalKind,
    size_bytes: u64,
}

#[derive(Debug, Serialize)]
struct RemovalOutcome {
    deleted: usize,
    failed: usize,
    step_count: usize,
    elapsed_ms: u128,
    failures: Vec<FailedRemoval>,
}

#[derive(Debug, Serialize)]
struct FailedRemoval {
    path: String,
    kind: utils::RemovalKind,
    error: String,
}

impl JsonRun {
    fn missing_path(root: &str) -> Self {
        Self {
            status: JsonStatus::MissingPath,
            root: root.to_string(),
            dry_run: false,
            mode: "all",
            exclude: Vec::new(),
            scan_elapsed_ms: 0,
            total_size_bytes: 0,
            target_count: 0,
            step_count: 0,
            targets: Vec::new(),
            removal: None,
            message: Some("path does not exist".to_string()),
        }
    }

    fn no_matches(
        root: &str,
        mode: ScanMode,
        exclude: &[String],
        dry_run: bool,
        scan_elapsed: std::time::Duration,
        step_count: usize,
    ) -> Self {
        Self::base(
            root,
            mode,
            exclude,
            dry_run,
            scan_elapsed,
            &[],
            0,
            step_count,
            JsonStatus::NoMatches,
        )
    }

    fn preview(
        root: &str,
        mode: ScanMode,
        exclude: &[String],
        scan_elapsed: std::time::Duration,
        targets: &[RemovalTarget],
        total_size_bytes: u64,
        step_count: usize,
    ) -> Self {
        Self::base(
            root,
            mode,
            exclude,
            true,
            scan_elapsed,
            targets,
            total_size_bytes,
            step_count,
            JsonStatus::Preview,
        )
    }

    fn cancelled(
        root: &str,
        mode: ScanMode,
        exclude: &[String],
        scan_elapsed: std::time::Duration,
        targets: &[RemovalTarget],
        total_size_bytes: u64,
        step_count: usize,
    ) -> Self {
        Self::base(
            root,
            mode,
            exclude,
            false,
            scan_elapsed,
            targets,
            total_size_bytes,
            step_count,
            JsonStatus::Cancelled,
        )
    }

    fn completed(
        root: &str,
        mode: ScanMode,
        exclude: &[String],
        scan_elapsed: std::time::Duration,
        targets: &[RemovalTarget],
        total_size_bytes: u64,
        step_count: usize,
        removal: RemovalOutcome,
    ) -> Self {
        let mut payload = Self::base(
            root,
            mode,
            exclude,
            false,
            scan_elapsed,
            targets,
            total_size_bytes,
            step_count,
            JsonStatus::Completed,
        );
        payload.removal = Some(removal);
        payload
    }

    fn base(
        root: &str,
        mode: ScanMode,
        exclude: &[String],
        dry_run: bool,
        scan_elapsed: std::time::Duration,
        targets: &[RemovalTarget],
        total_size_bytes: u64,
        step_count: usize,
        status: JsonStatus,
    ) -> Self {
        Self {
            status,
            root: root.to_string(),
            dry_run,
            mode: scan_mode_name(mode),
            exclude: exclude.to_vec(),
            scan_elapsed_ms: scan_elapsed.as_millis(),
            total_size_bytes,
            target_count: targets.len(),
            step_count,
            targets: targets.iter().map(JsonTarget::from).collect(),
            removal: None,
            message: None,
        }
    }
}

impl From<&RemovalTarget> for JsonTarget {
    fn from(target: &RemovalTarget) -> Self {
        Self {
            path: target.path().display().to_string(),
            kind: target.kind(),
            size_bytes: target.size(),
        }
    }
}

fn scan_mode_name(mode: ScanMode) -> &'static str {
    match mode {
        ScanMode::All => "all",
        ScanMode::DirectoriesOnly => "directories_only",
        ScanMode::FilesOnly => "files_only",
    }
}

fn build_delete_operations(
    entries: &[RemovalTarget],
) -> Vec<(utils::RemovalKind, String, DeleteOperation)> {
    let mut operations = Vec::new();

    for entry in entries {
        let label = target_label(entry.path());
        operations.extend(
            entry
                .delete_operations()
                .into_iter()
                .map(|operation| (entry.kind(), label.clone(), operation)),
        );
    }

    operations
}

fn execute_operation(
    operation: &(utils::RemovalKind, String, DeleteOperation),
) -> Result<(), std::io::Error> {
    match operation.2.action() {
        DeleteAction::DeleteFile => std::fs::remove_file(operation.2.path()),
        DeleteAction::DeleteDirectory => std::fs::remove_dir(operation.2.path()),
        DeleteAction::DeleteDirectoryIfEmpty => match std::fs::remove_dir(operation.2.path()) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        },
    }
}

fn format_progress_message(
    target: &str,
    operation: &DeleteOperation,
    progress_style: ProgressStyleKind,
) -> String {
    let path = shorten_path(&operation.path().display().to_string(), 72);
    match progress_style {
        ProgressStyleKind::Soft => format!("{}  •  {}", target, path),
        ProgressStyleKind::Minimal => format!("{} {}", target, path),
    }
}

fn target_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

fn shorten_path(path: &str, max_chars: usize) -> String {
    let count = path.chars().count();
    if count <= max_chars {
        return path.to_string();
    }

    if max_chars <= 3 {
        return "...".chars().take(max_chars).collect();
    }

    let tail_len = max_chars - 3;
    let tail: String = path
        .chars()
        .rev()
        .take(tail_len)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("...{}", tail)
}

fn progress_bar_style(progress_style: ProgressStyleKind) -> ProgressStyle {
    match progress_style {
        ProgressStyleKind::Soft => ProgressStyle::with_template(
            "{msg:.cyan}\n[{wide_bar:.cyan/bright_black}] {percent:>3. cyan}  {pos}/{len}  {elapsed_precise}",
        )
        .unwrap()
        .progress_chars("█▉▊▋▌▍▎▏ "),
        ProgressStyleKind::Minimal => ProgressStyle::with_template(
            "{msg:.cyan}\n[{wide_bar:.cyan/bright_black}] {percent:>3. cyan}  {elapsed_precise}",
        )
        .unwrap()
        .progress_chars("█▉▊▋▌▍▎▏ "),
    }
}

fn progress_message(message: &str, progress_style: ProgressStyleKind) -> String {
    match progress_style {
        ProgressStyleKind::Soft => message.to_string(),
        ProgressStyleKind::Minimal => shorten_path(message, 64),
    }
}

fn browser_roots(last_dir: Option<&Path>) -> Vec<BrowserRoot> {
    let mut roots = Vec::new();

    if let Some(path) = last_dir {
        push_browser_root(
            &mut roots,
            BrowserRoot::new(
                format!("Last selected: {}", path.display()),
                path.to_path_buf(),
            ),
        );
    }

    if let Ok(current_dir) = env::current_dir() {
        push_browser_root(
            &mut roots,
            BrowserRoot::new(
                format!("Current directory: {}", current_dir.display()),
                current_dir,
            ),
        );
    }

    if let Some(home_dir) = home_dir() {
        push_browser_root(
            &mut roots,
            BrowserRoot::new(format!("Home directory: {}", home_dir.display()), home_dir),
        );
    }

    for root in platform_roots() {
        push_browser_root(&mut roots, root);
    }

    roots
}

fn push_browser_root(roots: &mut Vec<BrowserRoot>, root: BrowserRoot) {
    if !roots.iter().any(|existing| existing.path == root.path) {
        roots.push(root);
    }
}

fn list_subdirectories(path: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut directories = fs::read_dir(path)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|entry_path| entry_path.is_dir())
        .collect::<Vec<_>>();

    directories.sort_by_key(|entry| {
        file_name_or_path(entry)
            .to_ascii_lowercase()
            .replace('\\', "/")
    });

    Ok(directories)
}

fn file_name_or_path(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

fn prompt_initial_browser_dir() -> Result<PathBuf, Box<dyn Error>> {
    loop {
        let fallback = env::current_dir()?;
        print!(
            "Enter initial directory [{}]: ",
            fallback.display()
        );
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
fn platform_roots() -> Vec<BrowserRoot> {
    vec![BrowserRoot::new(
        "Filesystem root: /".to_string(),
        PathBuf::from("/"),
    )]
}

struct BrowserRoot {
    label: String,
    path: PathBuf,
}

impl BrowserRoot {
    fn new(label: String, path: PathBuf) -> Self {
        Self { label, path }
    }
}

enum BrowserAction {
    UseCurrent,
    GoUp,
    ChangeRoot,
    Enter(PathBuf),
    Quit,
}

enum TuiMode {
    Browse,
    RootSelect,
    Preview,
}

enum PreviewAction {
    None,
    Back,
    Clean,
    Quit,
}

enum PreviewStatus {
    Loading { started_at: Instant },
    Ready,
    Failed(String),
}

struct PreviewScanResult {
    scan_id: u64,
    targets: Vec<RemovalTarget>,
    total_size: u64,
    scan_elapsed: std::time::Duration,
}

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
    preview_scan_elapsed: std::time::Duration,
    preview_status: PreviewStatus,
    preview_scan_rx: Option<Receiver<PreviewScanResult>>,
    preview_scan_id: u64,
}

impl TuiState {
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
            preview_scan_elapsed: std::time::Duration::from_secs(0),
            preview_status: PreviewStatus::Ready,
            preview_scan_rx: None,
            preview_scan_id: 0,
        }
    }
}

fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    cli: &Cli,
    state: &mut TuiState,
) -> Result<(), Box<dyn Error>> {
    state.roots = browser_roots(state.last_dir.as_deref());

    loop {
        process_preview_scan(state);
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

fn handle_browse_key(
    key: KeyCode,
    state: &mut TuiState,
    cli: &Cli,
) -> Result<bool, Box<dyn Error>> {
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
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
            match state.browser_actions.get(state.browser_index) {
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
            }
        }
        _ => {}
    }
    Ok(false)
}

fn handle_root_key(key: KeyCode, state: &mut TuiState) -> Result<bool, Box<dyn Error>> {
    match key {
        KeyCode::Esc
        | KeyCode::Backspace
        | KeyCode::Left
        | KeyCode::Char('h')
        | KeyCode::Char('b') => state.mode = TuiMode::Browse,
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

fn handle_preview_key(key: KeyCode, state: &mut TuiState) -> Result<PreviewAction, Box<dyn Error>> {
    match key {
        KeyCode::Char('q') => return Ok(PreviewAction::Quit),
        KeyCode::Esc | KeyCode::Char('b') => return Ok(PreviewAction::Back),
        KeyCode::Char('c') => {
            if matches!(state.preview_status, PreviewStatus::Ready)
                && !state.preview_targets.is_empty()
            {
                return Ok(PreviewAction::Clean);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if matches!(state.preview_status, PreviewStatus::Ready)
                && !state.preview_targets.is_empty()
            {
                state.preview_index = (state.preview_index + 1) % state.preview_targets.len();
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if matches!(state.preview_status, PreviewStatus::Ready)
                && !state.preview_targets.is_empty()
            {
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

fn build_preview(state: &mut TuiState, cli: &Cli) -> Result<(), Box<dyn Error>> {
    let mode = scan_mode_from_cli(cli);
    let config = ScanConfig::new(&cli.exclude, mode)?;
    let path = state.current_dir.clone();
    let scan_id = state.preview_scan_id.wrapping_add(1);
    let (tx, rx) = mpsc::channel();

    state.preview_scan_id = scan_id;
    state.preview_scan_rx = Some(rx);
    state.preview_targets.clear();
    state.preview_index = 0;
    state.preview_total_size = 0;
    state.preview_scan_elapsed = std::time::Duration::from_secs(0);
    state.preview_status = PreviewStatus::Loading {
        started_at: Instant::now(),
    };
    state.mode = TuiMode::Preview;

    thread::spawn(move || {
        let scan_start = Instant::now();
        let targets = collect_cleanup_targets(&path, &config);
        let _ = tx.send(PreviewScanResult {
            scan_id,
            total_size: calculate_entries_size(&targets),
            targets,
            scan_elapsed: scan_start.elapsed(),
        });
    });

    Ok(())
}

fn process_preview_scan(state: &mut TuiState) {
    let result = match state.preview_scan_rx.as_ref() {
        Some(rx) => match rx.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                state.preview_scan_rx = None;
                state.preview_status =
                    PreviewStatus::Failed("Background scan was interrupted.".to_string());
                None
            }
        },
        None => None,
    };

    if let Some(result) = result {
        if result.scan_id == state.preview_scan_id {
            state.preview_targets = result.targets;
            state.preview_total_size = result.total_size;
            state.preview_scan_elapsed = result.scan_elapsed;
            state.preview_index = 0;
            state.preview_status = PreviewStatus::Ready;
        }
        state.preview_scan_rx = None;
    }
}

fn cancel_preview(state: &mut TuiState) {
    state.preview_scan_rx = None;
    state.preview_targets.clear();
    state.preview_index = 0;
    state.preview_total_size = 0;
    state.preview_scan_elapsed = std::time::Duration::from_secs(0);
    state.preview_status = PreviewStatus::Ready;
    state.mode = TuiMode::Browse;
}

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

fn draw_tui(frame: &mut Frame, state: &TuiState) {
    match state.mode {
        TuiMode::Browse => draw_browse_view(frame, state),
        TuiMode::RootSelect => draw_root_view(frame, state),
        TuiMode::Preview => draw_preview_view(frame, state),
    }
}

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
        "Interactive cleanup browser",
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
        "Enter: preview   r: roots   Backspace/←: parent   q: quit",
    );
}

fn draw_root_view(frame: &mut Frame, state: &TuiState) {
    draw_browse_view(frame, state);
    let area = centered_rect(72, 62, frame.area());
    frame.render_widget(Clear, area);
    let modal = panel_block("Select Root");
    frame.render_widget(modal, area);

    let inner = area.inner(Margin {
        vertical: 1,
        horizontal: 2,
    });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(6), Constraint::Length(1)])
        .split(inner);

    let title = Paragraph::new(vec![Line::from(Span::styled(
        "Choose a root to jump to",
        Style::default()
            .fg(COLOR_TEXT_SOFT)
            .add_modifier(Modifier::DIM),
    ))]);
    frame.render_widget(title, chunks[0]);

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
        Span::raw("Enter: use root   Esc/b/Backspace/←: back   q: quit"),
    ]))
    .wrap(Wrap { trim: true });
    frame.render_widget(
        footer,
        chunks[2],
    );
}

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

    let preview_path = Paragraph::new(Line::from(vec![
        Span::styled("Preview Path  ", Style::default().fg(COLOR_ACCENT_ALT)),
        Span::raw(truncate_middle(
            &display_path(&state.current_dir),
            inner_width(list_chunks[0]).saturating_sub(16),
        )),
    ]))
    .block(panel_block("Current"))
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
        .block(panel_block("Planned Deletions"))
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

fn render_header(frame: &mut Frame, area: Rect, app: &str, mode: &str, path: &Path, status: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    let selected = if mode == "Preview" { 1 } else { 0 };
    render_segmented_control(frame, chunks[0], selected);

    let toolbar = Paragraph::new(Line::from(vec![
        Span::styled(
            app,
            Style::default()
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            mode,
            Style::default().fg(COLOR_ACCENT_ALT).add_modifier(Modifier::DIM),
        ),
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
        Span::styled("status", Style::default().fg(COLOR_SUCCESS)),
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

fn render_browse_sidebar(frame: &mut Frame, area: Rect, state: &TuiState) {
    let details = Paragraph::new(vec![
        Line::from(Span::styled(
            "Summary",
            Style::default()
                .fg(COLOR_ACCENT_ALT)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("Current: {}", file_name_or_path(&state.current_dir))),
        Line::from(format!(
            "Entries: {}",
            state.browser_actions.len().saturating_sub(4)
        )),
        Line::from(format!(
            "Parent: {}",
            if state.current_dir.parent().is_some() {
                "available"
            } else {
                "none"
            }
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Help",
            Style::default()
                .fg(COLOR_WARNING)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("Use arrows or j/k to move."),
        Line::from("Enter opens the selected directory."),
        Line::from("Select [Preview and use] to start a background scan."),
        Line::from("r opens the root picker."),
        Line::from("Backspace, ←, or h moves to the parent."),
    ])
    .block(panel_block("Details"))
    .wrap(Wrap { trim: true });
    frame.render_widget(details, area);
}

fn render_preview_sidebar(frame: &mut Frame, area: Rect, state: &TuiState) {
    let selected = state.preview_targets.get(state.preview_index);
    let lines = if let Some(entry) = selected {
        vec![
            Line::from(Span::styled(
                "Selection",
                Style::default()
                    .fg(COLOR_ACCENT_ALT)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("Type: {}", entry.label())),
            Line::from(format!("Size: {}", format_size(entry.size()))),
            Line::from(truncate_middle(
                &display_path(entry.path()),
                inner_width(area).saturating_sub(1),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Help",
                Style::default()
                    .fg(COLOR_WARNING)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("Review the planned targets."),
            Line::from("Use ↑/↓ or j/k to inspect each entry."),
            Line::from("Press c to start cleanup."),
            Line::from("Press Esc or b to return."),
        ]
    } else {
        let mut lines = vec![
            Line::from(Span::styled(
                "Selection",
                Style::default()
                    .fg(COLOR_ACCENT_ALT)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("Type: -"),
            Line::from("Size: -"),
            Line::from("No planned deletions."),
            Line::from(""),
        ];
        lines.extend(preview_status_lines(state, inner_width(area).saturating_sub(1)));
        lines.extend([
            Line::from(Span::styled(
                "Help",
                Style::default()
                    .fg(COLOR_WARNING)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("Review the planned targets."),
            Line::from("Use ↑/↓ or j/k to inspect each entry."),
            Line::from("Press c to start cleanup."),
            Line::from("Press Esc or b to return."),
        ]);
        lines
    };
    let selection = Paragraph::new(lines)
        .block(panel_block("Details"))
        .wrap(Wrap { trim: true });
    frame.render_widget(selection, area);
}

fn render_action_footer(frame: &mut Frame, area: Rect, text: &str) {
    let footer = Paragraph::new(Line::from(vec![
        Span::styled("keys ", Style::default().fg(COLOR_WARNING)),
        Span::raw(text),
    ]))
        .block(separator_block(Borders::TOP))
        .wrap(Wrap { trim: true });
    frame.render_widget(footer, area);
}

fn render_segmented_control(frame: &mut Frame, area: Rect, selected: usize) {
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("[ ", Style::default().fg(COLOR_BORDER)),
        Span::styled("Browse", segmented_text_style(selected == 0)),
        Span::styled(" | ", Style::default().fg(COLOR_BORDER)),
        Span::styled("Preview", segmented_text_style(selected == 1)),
        Span::styled(" ]", Style::default().fg(COLOR_BORDER)),
    ]);
    let paragraph = Paragraph::new(line).wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

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

fn separator_block(borders: Borders) -> Block<'static> {
    Block::default()
        .borders(borders)
        .border_style(Style::default().fg(COLOR_BORDER))
}

fn preview_status_line(state: &TuiState) -> String {
    match &state.preview_status {
        PreviewStatus::Loading { started_at } => {
            format!("scanning in background • elapsed {:.1?}", started_at.elapsed())
        }
        PreviewStatus::Ready => format!(
            "{} target(s) • {} • scan {:.2?}",
            state.preview_targets.len(),
            format_size(state.preview_total_size),
            state.preview_scan_elapsed
        ),
        PreviewStatus::Failed(message) => format!("scan failed • {}", message),
    }
}

fn preview_status_lines(state: &TuiState, max_width: usize) -> Vec<Line<'static>> {
    match &state.preview_status {
        PreviewStatus::Loading { started_at } => vec![
            Line::from(Span::styled(
                "Background Scan",
                Style::default()
                    .fg(COLOR_SUCCESS)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(truncate_middle(
                &format!("Scanning… elapsed {:.1?}", started_at.elapsed()),
                max_width,
            )),
            Line::from("Targets and total size will appear when ready."),
            Line::from(""),
        ],
        PreviewStatus::Failed(message) => vec![
            Line::from(Span::styled(
                "Background Scan",
                Style::default()
                    .fg(COLOR_WARNING)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(truncate_middle(message, max_width)),
            Line::from(""),
        ],
        PreviewStatus::Ready => Vec::new(),
    }
}

fn browser_action_label(action: &BrowserAction, current_dir: &Path, max_width: usize) -> String {
    match action {
        BrowserAction::UseCurrent => truncate_middle(
            &format!("[Preview and use] {}", display_path(current_dir)),
            max_width,
        ),
        BrowserAction::GoUp => "[Go up] ..".to_string(),
        BrowserAction::ChangeRoot => "[Change root]".to_string(),
        BrowserAction::Enter(path) => truncate_middle(&format!("{}/", file_name_or_path(path)), max_width),
        BrowserAction::Quit => "[Quit]".to_string(),
    }
}

fn format_preview_entry_lines(entry: &RemovalTarget, max_width: usize) -> Vec<Line<'static>> {
    if max_width <= 48 {
        return format_preview_entry_compact(entry, max_width);
    }

    vec![Line::from(format_preview_entry_single_line(entry, max_width))]
}

fn format_preview_entry_compact(entry: &RemovalTarget, max_width: usize) -> Vec<Line<'static>> {
    let label_line = truncate_middle(
        &format!("[{}] {}", entry.label(), display_path(entry.path())),
        max_width,
    );
    let size_line = truncate_middle(&format!("size: {}", format_size(entry.size())), max_width);

    vec![Line::from(label_line), Line::from(size_line)]
}

fn format_preview_entry_single_line(entry: &RemovalTarget, max_width: usize) -> String {
    let size = format!("({})", format_size(entry.size()));
    let prefix = format!("[{}] ", entry.label());
    if max_width <= prefix.len() + size.len() + 1 {
        return truncate_middle(&format!("{prefix}{}", display_path(entry.path())), max_width);
    }

    let path_width = max_width.saturating_sub(prefix.len() + size.len() + 1);
    let path = truncate_middle(&display_path(entry.path()), path_width);
    format!("{prefix}{path} {size}")
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

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

fn inner_width(area: Rect) -> usize {
    area.width.saturating_sub(2) as usize
}

fn inner_height(area: Rect) -> usize {
    area.height.saturating_sub(2) as usize
}

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
