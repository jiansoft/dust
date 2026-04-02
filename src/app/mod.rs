//! High-level application flow for scanning, previewing, and deleting targets.

use crate::cleanup::{
    DeleteAction, DeleteOperation, RemovalKind, RemovalTarget, ScanConfig, ScanMode,
    calculate_entries_size, collect_cleanup_targets, format_size,
};
use crate::cli::{Cli, ProgressStyleKind};
use crate::interactive;
use dialoguer::Confirm;
use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;
use std::{error::Error, path::Path, time::Instant};

type PlannedOperation = (RemovalKind, String, DeleteOperation);

/// Runs the application in interactive or direct path mode based on the CLI.
pub(crate) fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    if cli.json && cli.path.is_none() {
        return Err(
            "`--json` requires a path argument and does not support interactive mode".into(),
        );
    }

    if cli.path.is_none() && !cli.json {
        return interactive::run_interactive(cli);
    }

    run_once(&cli, cli.path.clone().unwrap_or_default())
}

/// Executes one scan-and-clean cycle for the given folder.
pub(crate) fn run_once(cli: &Cli, folder: String) -> Result<(), Box<dyn Error>> {
    let mode = scan_mode_from_cli(cli);
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

    let operations = (cli.json || cli.dry_run).then(|| build_delete_operations(&to_delete));
    let step_count = operations.as_ref().map_or(0, Vec::len);

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

    let outcome = if let Some(operations) = operations.as_ref() {
        execute_removal_with_operations(
            &to_delete,
            operations,
            cli.json,
            cli.quiet,
            cli.no_progress,
            cli.progress_style,
        )
    } else {
        execute_removal(
            &to_delete,
            cli.json,
            cli.quiet,
            cli.no_progress,
            cli.progress_style,
        )
    };
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

/// Maps CLI switches to the internal scan mode.
pub(crate) fn scan_mode_from_cli(cli: &Cli) -> ScanMode {
    if cli.dirs_only {
        ScanMode::DirectoriesOnly
    } else if cli.files_only {
        ScanMode::FilesOnly
    } else {
        ScanMode::All
    }
}

/// Prints the planned cleanup items for console mode.
fn print_plan(entries: &[RemovalTarget], scan_elapsed: std::time::Duration) {
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

/// Asks the user to confirm deletion unless confirmation has been skipped.
fn confirm_deletion(skip_confirmation: bool) -> Result<bool, Box<dyn Error>> {
    if skip_confirmation {
        return Ok(true);
    }

    Ok(Confirm::new()
        .with_prompt("Delete these items?")
        .default(false)
        .interact()?)
}

/// Executes all delete operations derived from the provided cleanup targets.
pub(crate) fn execute_removal(
    entries: &[RemovalTarget],
    json_mode: bool,
    quiet: bool,
    no_progress: bool,
    progress_style: ProgressStyleKind,
) -> RemovalOutcome {
    let operations = build_delete_operations(entries);
    execute_removal_with_operations(
        entries,
        &operations,
        json_mode,
        quiet,
        no_progress,
        progress_style,
    )
}

/// Executes a pre-built set of delete operations derived from cleanup targets.
fn execute_removal_with_operations(
    entries: &[RemovalTarget],
    operations: &[PlannedOperation],
    json_mode: bool,
    quiet: bool,
    no_progress: bool,
    progress_style: ProgressStyleKind,
) -> RemovalOutcome {
    let start_time = Instant::now();
    let mut success = 0usize;
    let mut failed = 0usize;
    let mut failures = Vec::new();

    let show_progress = !json_mode && !quiet && !no_progress;
    let progress = show_progress.then(|| {
        let bar = ProgressBar::new(operations.len() as u64);
        bar.set_style(progress_bar_style(progress_style));
        bar.set_message(progress_message("Preparing delete plan", progress_style));
        bar
    });

    for operation in operations {
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

/// Summary of a cleanup run.
#[derive(Debug, Serialize)]
pub(crate) struct RemovalOutcome {
    /// Number of successful delete steps.
    pub(crate) deleted: usize,
    /// Number of failed delete steps.
    pub(crate) failed: usize,
    /// Number of delete steps attempted.
    pub(crate) step_count: usize,
    /// Total elapsed time in milliseconds.
    pub(crate) elapsed_ms: u128,
    /// Detailed failures for steps that did not complete.
    pub(crate) failures: Vec<FailedRemoval>,
}

/// Details for a delete step that failed.
#[derive(Debug, Serialize)]
pub(crate) struct FailedRemoval {
    /// Path that failed.
    pub(crate) path: String,
    /// Cleanup kind that produced the failing step.
    pub(crate) kind: RemovalKind,
    /// Stringified error message.
    pub(crate) error: String,
}

/// JSON payload emitted when `--json` is enabled.
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

/// Status values used in JSON responses.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum JsonStatus {
    /// The requested root path does not exist.
    MissingPath,
    /// Scan completed without finding any cleanup targets.
    NoMatches,
    /// A dry-run preview response.
    Preview,
    /// The user cancelled cleanup after preview.
    Cancelled,
    /// Cleanup completed and a final result is available.
    Completed,
}

/// JSON representation of a single cleanup target.
#[derive(Debug, Serialize)]
struct JsonTarget {
    path: String,
    kind: RemovalKind,
    size_bytes: u64,
}

impl JsonRun {
    /// Builds a payload for a missing-path result.
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

    /// Builds a payload for a completed scan that found no matches.
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

    /// Builds a payload for dry-run preview output.
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

    /// Builds a payload for a user-cancelled cleanup run.
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

    /// Builds a payload for a completed cleanup run.
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

    /// Builds the common JSON fields shared by all payload variants.
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

/// Converts a cleanup target into its JSON representation.
impl From<&RemovalTarget> for JsonTarget {
    fn from(target: &RemovalTarget) -> Self {
        Self {
            path: target.path().display().to_string(),
            kind: target.kind(),
            size_bytes: target.size(),
        }
    }
}

/// Pretty-prints a JSON payload to stdout.
fn print_json(payload: &JsonRun) -> Result<(), Box<dyn Error>> {
    println!("{}", serde_json::to_string_pretty(payload)?);
    Ok(())
}

/// Converts a scan mode into the serialized JSON string value.
fn scan_mode_name(mode: ScanMode) -> &'static str {
    match mode {
        ScanMode::All => "all",
        ScanMode::DirectoriesOnly => "directories_only",
        ScanMode::FilesOnly => "files_only",
    }
}

/// Expands cleanup targets into executable delete operations.
fn build_delete_operations(entries: &[RemovalTarget]) -> Vec<PlannedOperation> {
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

/// Executes one low-level filesystem delete operation.
fn execute_operation(operation: &PlannedOperation) -> Result<(), std::io::Error> {
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

/// Formats the progress message shown for the current delete step.
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

/// Derives a short label for a target path.
fn target_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

/// Truncates a long path from the left while preserving the tail.
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

/// Builds the progress-bar style for the selected output mode.
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

/// Builds the initial progress message for the selected output mode.
fn progress_message(message: &str, progress_style: ProgressStyleKind) -> String {
    match progress_style {
        ProgressStyleKind::Soft => message.to_string(),
        ProgressStyleKind::Minimal => shorten_path(message, 64),
    }
}
