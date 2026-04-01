mod utils;

use clap::Parser;
use clap::ValueEnum;
use dialoguer::{Confirm, Input};
use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;
use std::{error::Error, path::Path, time::Instant};
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
    loop {
        let Some(folder) = prompt_for_root()? else {
            println!("Exiting.");
            return Ok(());
        };

        run_once(&cli, folder)?;
        println!();
    }
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

fn prompt_for_root() -> Result<Option<String>, Box<dyn Error>> {
    let input: String = Input::new()
        .with_prompt("Enter directory to clean (q to quit)")
        .allow_empty(true)
        .interact_text()?;

    let trimmed = input.trim();
    if trimmed.is_empty() || matches!(trimmed.to_ascii_lowercase().as_str(), "q" | "quit" | "exit")
    {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
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
