//! Core scan, classification, and delete-planning logic for cleanup targets.

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Serialize;
#[cfg(test)]
use std::io;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};
use walkdir::{IntoIter, WalkDir};

/// Directory names that are commonly used as build outputs, dependency caches,
/// or generated artifacts and are usually safe to clean.
///
/// Common language and tool mappings:
/// - `bin`, `obj`: C#, .NET, MSBuild
/// - `node_modules`: Node.js, JavaScript, TypeScript
/// - `target`: Rust, Maven, general build output
/// - `zig-cache`, `.zig-cache`, `zig-out`: Zig
/// - `build`, `out`: Java, Kotlin, Gradle, Android, C/C++, general IDE output
/// - `.gradle`: Gradle
/// - `.next`: Next.js
/// - `.nuxt`: Nuxt
/// - `.svelte-kit`: SvelteKit
/// - `.turbo`: Turborepo
/// - `.parcel-cache`: Parcel
/// - `.dart_tool`: Dart, Flutter
/// - `.build`: Swift Package Manager
/// - `_build`: Elixir, Erlang rebar/mix-style output
/// - `__pycache__`, `.pytest_cache`, `.mypy_cache`, `.ruff_cache`: Python
/// - `CMakeFiles`: CMake, C/C++
const DIRECTORY_NAMES: &[&str] = &[
    // C#, .NET, MSBuild
    "bin",
    "obj",
    // Node.js and frontend toolchains
    "node_modules",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".parcel-cache",
    // Rust and JVM/Maven-style outputs
    "target",
    // Zig
    "zig-cache",
    ".zig-cache",
    "zig-out",
    // Java, Kotlin, Gradle, Android, and general IDE output
    "build",
    "out",
    ".gradle",
    // Dart / Flutter
    ".dart_tool",
    // Swift Package Manager
    ".build",
    // Elixir / Erlang
    "_build",
    // Python
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    // CMake / C / C++
    "CMakeFiles",
];

const LOG_DIRECTORY_NAMES: &[&str] = &["log", "logs"];
/// Directory names that are skipped entirely during traversal because they are
/// metadata, protected dependency trees, or generated outputs that should not
/// be proposed for cleanup.
const FAST_SKIP_DIRECTORY_NAMES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    // Generated Rust API documentation output.
    "rustdoc",
    ".idea",
    ".vscode",
    ".cache",
    ".venv",
    ".pnpm-store",
    ".yarn",
    ".nuget",
    ".terraform",
    ".serverless",
    ".aws-sam",
    "coverage",
    "vendor",
    "third_party",
    "deploy",
    "Pods",
];
const SAFE_FILE_EXTENSIONS: &[&str] = &["pdb", "ilk", "o", "obj"];
const BUILD_DIR_ONLY_FILE_EXTENSIONS: &[&str] = &["so", "a", "lib", "dll", "exe", "wasm"];
const LOG_FILE_EXTENSIONS: &[&str] = &["log", "txt"];

/// Classifies the type of cleanup work represented by a [`RemovalTarget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalKind {
    /// A directory that can be removed recursively.
    Directory,
    /// A `log` or `logs` directory where only selected log-like files are removed.
    LogDirectory,
    /// A synthetic target that groups removable files from the same directory.
    FileGroup,
}

/// Describes a single filesystem mutation that will be executed during cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteAction {
    /// Remove a file.
    File,
    /// Remove a directory unconditionally.
    Directory,
    /// Remove a directory only if it is empty at execution time.
    DirectoryIfEmpty,
}

/// A normalized delete step derived from a [`RemovalTarget`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteOperation {
    path: PathBuf,
    action: DeleteAction,
}

impl DeleteOperation {
    /// Creates a new delete operation for the given path and action.
    pub fn new(path: PathBuf, action: DeleteAction) -> Self {
        Self { path, action }
    }

    /// Returns the filesystem path affected by this operation.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the delete action to execute for [`Self::path`].
    pub fn action(&self) -> DeleteAction {
        self.action
    }
}

/// Controls whether scanning should consider directories, files, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    /// Include removable directories and removable grouped files.
    All,
    /// Include only removable directories.
    DirectoriesOnly,
    /// Include only removable grouped files.
    FilesOnly,
}

/// Scanning options used when traversing a workspace.
#[derive(Debug, Clone)]
pub struct ScanConfig {
    exclude_set: GlobSet,
    has_exclusions: bool,
    mode: ScanMode,
}

impl ScanConfig {
    /// Builds a scan configuration from glob exclusions and a scan mode.
    pub fn new(exclusions: &[String], mode: ScanMode) -> Result<Self, globset::Error> {
        let mut builder = GlobSetBuilder::new();
        for exclusion in exclusions {
            builder.add(Glob::new(exclusion)?);
        }
        Ok(Self {
            exclude_set: builder.build()?,
            has_exclusions: !exclusions.is_empty(),
            mode,
        })
    }

    /// Returns the active scan mode.
    pub fn mode(&self) -> ScanMode {
        self.mode
    }

    fn is_excluded(&self, path: &Path) -> bool {
        self.has_exclusions && self.exclude_set.is_match(path)
    }
}

/// A candidate cleanup item shown to the user and later expanded into delete steps.
#[derive(Debug, Clone, Serialize)]
pub struct RemovalTarget {
    path: PathBuf,
    kind: RemovalKind,
    size: Option<u64>,
    grouped_paths: Option<Vec<PathBuf>>,
}

impl RemovalTarget {
    /// Creates a sized target for a directory-like cleanup item.
    pub fn new(path: PathBuf, kind: RemovalKind, size: u64) -> Self {
        Self {
            path,
            kind,
            size: Some(size),
            grouped_paths: None,
        }
    }

    /// Creates a target whose size will be computed later.
    pub fn new_unsized(path: PathBuf, kind: RemovalKind) -> Self {
        Self {
            path,
            kind,
            size: None,
            grouped_paths: None,
        }
    }

    /// Creates a grouped-file target for removable files that share a parent directory.
    pub fn new_group(
        path: PathBuf,
        size: Option<u64>,
        grouped_paths: Option<Vec<PathBuf>>,
    ) -> Self {
        Self {
            path,
            kind: RemovalKind::FileGroup,
            size,
            grouped_paths,
        }
    }

    /// Returns the path represented by this cleanup target.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the target kind.
    pub fn kind(&self) -> RemovalKind {
        self.kind
    }

    /// Returns the current size in bytes, or `0` if the size has not been computed yet.
    pub fn size(&self) -> u64 {
        self.size.unwrap_or(0)
    }

    /// Returns the current size in bytes if it has already been computed.
    pub fn size_bytes(&self) -> Option<u64> {
        self.size
    }

    /// Updates the cached size stored on this target.
    pub fn set_size(&mut self, size: u64) {
        self.size = Some(size);
    }

    /// Returns the short UI label used when rendering this target.
    pub fn label(&self) -> &'static str {
        match self.kind {
            RemovalKind::Directory | RemovalKind::LogDirectory => "DIR ",
            RemovalKind::FileGroup => "FILES",
        }
    }

    /// Expands the target into concrete delete operations.
    pub fn delete_operations(&self) -> Vec<DeleteOperation> {
        match self.kind {
            RemovalKind::Directory => collect_directory_delete_operations(&self.path),
            RemovalKind::LogDirectory => collect_log_directory_delete_operations(&self.path),
            RemovalKind::FileGroup => grouped_file_paths(self)
                .iter()
                .map(|path| DeleteOperation::new(path.to_path_buf(), DeleteAction::File))
                .collect(),
        }
    }
}

/// Counts the visible contents of a selected target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetContentStats {
    /// Number of files contained in the target.
    pub file_count: usize,
    /// Number of subdirectories contained in the target.
    pub dir_count: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct LogDirectorySummary {
    cleanup_size: u64,
    has_files: bool,
    has_cleanup_files: bool,
}

/// Collects matching cleanup targets and computes their sizes eagerly.
pub fn collect_cleanup_targets(root: &Path, config: &ScanConfig) -> Vec<RemovalTarget> {
    collect_cleanup_targets_with_size(root, config, true)
}

/// Collects matching cleanup targets without computing sizes up front.
///
/// This is intended for responsive previews where sizes are filled in later.
pub fn collect_cleanup_targets_fast(root: &Path, config: &ScanConfig) -> Vec<RemovalTarget> {
    collect_cleanup_targets_with_size(root, config, false)
}

/// Returns a directory-like cleanup target if the path itself is removable.
fn removable_directory_target(
    path: &Path,
    mode: ScanMode,
    with_size: bool,
) -> Option<RemovalTarget> {
    if !matches!(mode, ScanMode::All | ScanMode::DirectoriesOnly) {
        return None;
    }

    if matches_name(path.file_name(), DIRECTORY_NAMES) {
        return Some(if with_size {
            RemovalTarget::new(
                path.to_path_buf(),
                RemovalKind::Directory,
                directory_size(path),
            )
        } else {
            RemovalTarget::new_unsized(path.to_path_buf(), RemovalKind::Directory)
        });
    }

    if matches_name(path.file_name(), LOG_DIRECTORY_NAMES) {
        let summary = log_directory_summary(path);
        if summary.has_cleanup_files || !summary.has_files {
            return Some(if with_size {
                RemovalTarget::new(
                    path.to_path_buf(),
                    RemovalKind::LogDirectory,
                    summary.cleanup_size,
                )
            } else {
                RemovalTarget::new_unsized(path.to_path_buf(), RemovalKind::LogDirectory)
            });
        }
    }

    None
}

/// Shared implementation for eager and deferred target collection.
fn collect_cleanup_targets_with_size(
    root: &Path,
    config: &ScanConfig,
    with_size: bool,
) -> Vec<RemovalTarget> {
    if config.is_excluded(root) || matches_name(root.file_name(), FAST_SKIP_DIRECTORY_NAMES) {
        return Vec::new();
    }

    if let Some(target) = removable_directory_target(root, config.mode(), with_size) {
        return vec![target];
    }

    let mut targets = Vec::new();
    let mut grouped_files: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let mut grouped_dirs: HashSet<PathBuf> = HashSet::new();
    let mut grouped_sizes: HashMap<PathBuf, u64> = HashMap::new();
    let mut iter = WalkDir::new(root).min_depth(1).into_iter();
    let mut build_context_stack = vec![should_remove_dir(root)];

    while let Some(next) = iter.next() {
        let entry = match next {
            Ok(entry) => entry,
            Err(_) => continue,
        };

        build_context_stack.truncate(entry.depth());
        let inside_build_dir = build_context_stack.last().copied().unwrap_or(false);

        let path = entry.path();
        let is_dir = entry.file_type().is_dir();
        let is_file = entry.file_type().is_file();

        if is_dir && matches_name(path.file_name(), FAST_SKIP_DIRECTORY_NAMES) {
            iter.skip_current_dir();
            continue;
        }

        if config.is_excluded(path) {
            if is_dir {
                iter.skip_current_dir();
            }
            continue;
        }

        if is_dir && let Some(target) = removable_directory_target(path, config.mode(), with_size) {
            targets.push(target);
            iter.skip_current_dir();
            continue;
        }

        if is_dir && matches_name(path.file_name(), LOG_DIRECTORY_NAMES) {
            iter.skip_current_dir();
            continue;
        }

        if is_file
            && should_remove_file(path, inside_build_dir)
            && matches!(config.mode(), ScanMode::All | ScanMode::FilesOnly)
        {
            let group_path = path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| path.to_path_buf());
            if with_size {
                grouped_files
                    .entry(group_path.clone())
                    .or_default()
                    .push(path.to_path_buf());
                *grouped_sizes.entry(group_path).or_default() += file_size(path);
            } else {
                grouped_dirs.insert(group_path);
            }
        }

        if is_dir {
            build_context_stack.push(inside_build_dir || should_remove_dir(path));
        }
    }

    for (group_path, paths) in grouped_files {
        let size = with_size.then(|| grouped_sizes.get(&group_path).copied().unwrap_or(0));
        targets.push(RemovalTarget::new_group(group_path, size, Some(paths)));
    }

    for group_path in grouped_dirs {
        targets.push(RemovalTarget::new_group(group_path, None, None));
    }

    targets.sort_by(|left, right| left.path().cmp(right.path()));
    targets
}

/// Sums the currently known sizes of all targets.
pub fn calculate_entries_size(entries: &[RemovalTarget]) -> u64 {
    entries.iter().map(RemovalTarget::size).sum()
}

/// Computes a target size from a path and removal kind.
pub fn compute_target_size_for_path(path: &Path, kind: RemovalKind) -> u64 {
    match kind {
        RemovalKind::Directory => directory_size(path),
        RemovalKind::LogDirectory => log_directory_summary(path).cleanup_size,
        RemovalKind::FileGroup => collect_grouped_file_paths(path)
            .iter()
            .map(|entry_path| file_size(entry_path))
            .sum(),
    }
}

/// Counts the files and folders contained by a target path and kind.
pub fn summarize_target_contents_for_path(path: &Path, kind: RemovalKind) -> TargetContentStats {
    match kind {
        RemovalKind::Directory | RemovalKind::LogDirectory => {
            let iter: IntoIter = WalkDir::new(path).min_depth(1).into_iter();
            let mut file_count = 0usize;
            let mut dir_count = 0usize;

            for entry in iter.filter_map(|entry| entry.ok()) {
                if entry.file_type().is_file() {
                    file_count += 1;
                } else if entry.file_type().is_dir() {
                    dir_count += 1;
                }
            }

            TargetContentStats {
                file_count,
                dir_count,
            }
        }
        RemovalKind::FileGroup => TargetContentStats {
            file_count: collect_grouped_file_paths(path).len(),
            dir_count: 0,
        },
    }
}

/// Formats a byte size using human-readable binary units.
pub fn format_size(size: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if size >= GB {
        format!("{:.2} GB", size as f64 / GB as f64)
    } else if size >= MB {
        format!("{:.2} MB", size as f64 / MB as f64)
    } else if size >= KB {
        format!("{:.2} KB", size as f64 / KB as f64)
    } else {
        format!("{} B", size)
    }
}

/// Returns whether a directory name is one of the known removable output folders.
fn should_remove_dir(path: &Path) -> bool {
    matches_name(path.file_name(), DIRECTORY_NAMES)
}

/// Returns whether a file should be treated as a removable generated artifact.
fn should_remove_file(path: &Path, inside_build_dir: bool) -> bool {
    let Some(ext) = path.extension().and_then(OsStr::to_str) else {
        return false;
    };

    if SAFE_FILE_EXTENSIONS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(ext))
    {
        return true;
    }

    BUILD_DIR_ONLY_FILE_EXTENSIONS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(ext))
        && inside_build_dir
}

/// Returns whether a file under a log directory should be removed.
fn should_remove_log_file(path: &Path) -> bool {
    path.extension().and_then(OsStr::to_str).is_some_and(|ext| {
        LOG_FILE_EXTENSIONS
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(ext))
    })
}

/// Returns whether a name matches any candidate string ignoring ASCII case.
fn matches_name(name: Option<&OsStr>, candidates: &[&str]) -> bool {
    name.and_then(OsStr::to_str).is_some_and(|value| {
        candidates
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(value))
    })
}

/// Returns the concrete file paths represented by a grouped-file target.
fn grouped_file_paths(target: &RemovalTarget) -> Cow<'_, [PathBuf]> {
    match target.grouped_paths.as_deref() {
        Some(paths) => Cow::Borrowed(paths),
        None => Cow::Owned(collect_grouped_file_paths(&target.path)),
    }
}

/// Collects removable files directly contained in the given directory.
fn collect_grouped_file_paths(path: &Path) -> Vec<PathBuf> {
    let inside_build_dir = path.ancestors().any(should_remove_dir);

    fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|entry_path| {
            entry_path.is_file() && should_remove_file(entry_path, inside_build_dir)
        })
        .collect()
}

/// Returns the file size in bytes, defaulting to zero if metadata lookup fails.
fn file_size(path: &Path) -> u64 {
    path.metadata().map(|metadata| metadata.len()).unwrap_or(0)
}

/// Recursively sums the sizes of all files contained in a directory.
fn directory_size(path: &Path) -> u64 {
    let iter: IntoIter = WalkDir::new(path).into_iter();
    iter.filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .sum()
}

/// Scans a log directory once and records removable size and file presence.
fn log_directory_summary(path: &Path) -> LogDirectorySummary {
    let iter: IntoIter = WalkDir::new(path).into_iter();
    let mut summary = LogDirectorySummary::default();

    for entry in iter.filter_map(|entry| entry.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }

        summary.has_files = true;
        let entry_path = entry.path();
        if should_remove_log_file(entry_path) {
            summary.has_cleanup_files = true;
            summary.cleanup_size += entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        }
    }

    summary
}

#[cfg(test)]
/// Removes only the cleanup-eligible contents of a log directory.
fn cleanup_log_directory(path: &Path) -> io::Result<()> {
    let iter: IntoIter = WalkDir::new(path).contents_first(true).into_iter();
    for entry in iter.filter_map(|entry| entry.ok()) {
        let entry_path = entry.path();
        if entry.file_type().is_file() && should_remove_log_file(entry_path) {
            fs::remove_file(entry_path)?;
            continue;
        }
        if entry.file_type().is_dir() && entry_path != path && directory_is_empty(entry_path)? {
            fs::remove_dir(entry_path)?;
        }
    }
    if directory_is_empty(path)? {
        fs::remove_dir(path)?;
    }
    Ok(())
}

#[cfg(test)]
/// Returns whether a directory is empty.
fn directory_is_empty(path: &Path) -> io::Result<bool> {
    Ok(fs::read_dir(path)?.next().is_none())
}

/// Expands a removable directory into recursive delete operations.
fn collect_directory_delete_operations(path: &Path) -> Vec<DeleteOperation> {
    let iter: IntoIter = WalkDir::new(path).contents_first(true).into_iter();
    iter.filter_map(|entry| entry.ok())
        .map(|entry| {
            let action = if entry.file_type().is_dir() {
                DeleteAction::Directory
            } else {
                DeleteAction::File
            };
            DeleteOperation::new(entry.into_path(), action)
        })
        .collect()
}

/// Expands a log directory into selective delete operations.
fn collect_log_directory_delete_operations(path: &Path) -> Vec<DeleteOperation> {
    let iter: IntoIter = WalkDir::new(path).contents_first(true).into_iter();
    iter.filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let entry_path = entry.into_path();
            if entry_path.is_file() && should_remove_log_file(&entry_path) {
                return Some(DeleteOperation::new(entry_path, DeleteAction::File));
            }
            if entry_path.is_dir() {
                return Some(DeleteOperation::new(
                    entry_path,
                    DeleteAction::DirectoryIfEmpty,
                ));
            }
            None
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{self, File},
        io::Write,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn format_size_uses_human_readable_units() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2 * 1024), "2.00 KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.00 MB");
    }

    #[test]
    fn collect_cleanup_targets_matches_known_directories_and_files() {
        let root = create_temp_dir("collect_cleanup_targets");
        let target_dir = root.join("project").join("target");
        let output_file = target_dir.join("app.exe");
        fs::create_dir_all(&target_dir).unwrap();
        fs::create_dir_all(output_file.parent().unwrap()).unwrap();
        write_file(&target_dir.join("artifact.bin"), b"1234");
        write_file(&output_file, b"5678");

        let config = ScanConfig::new(&[], ScanMode::All).unwrap();
        let results = collect_cleanup_targets(&root, &config);

        assert_eq!(results.len(), 1);
        assert!(results.iter().any(|entry| entry.path() == target_dir));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exclusions_use_glob_matching() {
        let root = create_temp_dir("exclusions_use_glob_matching");
        let keep_target = root.join("vendor").join("tool").join("target");
        let remove_target = root.join("app").join("target");
        fs::create_dir_all(&keep_target).unwrap();
        fs::create_dir_all(&remove_target).unwrap();

        let patterns = vec![format!("{}/vendor/**", root.to_string_lossy())];
        let config = ScanConfig::new(&patterns, ScanMode::All).unwrap();
        let results = collect_cleanup_targets(&root, &config);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path(), remove_target);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fast_skip_directories_are_not_scanned() {
        let root = create_temp_dir("fast_skip_directories_are_not_scanned");
        let skipped_target = root.join(".git").join("nested").join("target");
        let normal_target = root.join("app").join("target");
        fs::create_dir_all(&skipped_target).unwrap();
        fs::create_dir_all(&normal_target).unwrap();

        let config = ScanConfig::new(&[], ScanMode::All).unwrap();
        let results = collect_cleanup_targets_fast(&root, &config);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path(), normal_target);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rustdoc_directories_are_not_scanned() {
        let root = create_temp_dir("rustdoc_directories_are_not_scanned");
        let skipped_target = root.join("rustdoc").join("nested").join("target");
        let normal_target = root.join("app").join("target");
        fs::create_dir_all(&skipped_target).unwrap();
        fs::create_dir_all(&normal_target).unwrap();

        let config = ScanConfig::new(&[], ScanMode::All).unwrap();
        let results = collect_cleanup_targets_fast(&root, &config);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path(), normal_target);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn virtualenv_directories_are_not_scanned() {
        let root = create_temp_dir("virtualenv_directories_are_not_scanned");
        let skipped_target = root.join(".venv").join("nested").join("target");
        let normal_target = root.join("app").join("target");
        fs::create_dir_all(&skipped_target).unwrap();
        fs::create_dir_all(&normal_target).unwrap();

        let config = ScanConfig::new(&[], ScanMode::All).unwrap();
        let results = collect_cleanup_targets_fast(&root, &config);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path(), normal_target);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn virtualenv_root_is_not_scanned() {
        let root = create_temp_dir("virtualenv_root_is_not_scanned");
        let venv = root.join(".venv");
        let skipped_target = venv.join("nested").join("target");
        fs::create_dir_all(&skipped_target).unwrap();

        let config = ScanConfig::new(&[], ScanMode::All).unwrap();
        let results = collect_cleanup_targets_fast(&venv, &config);

        assert!(results.is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn binary_files_outside_build_directories_are_not_removed() {
        let root = create_temp_dir("binary_files_outside_build_directories_are_not_removed");
        let deploy_exe = root.join("deploy").join("plink.exe");
        write_file(&deploy_exe, b"1234");

        let config = ScanConfig::new(&[], ScanMode::All).unwrap();
        let results = collect_cleanup_targets_fast(&root, &config);

        assert!(results.is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn binary_files_inside_build_directories_are_removed() {
        let root = create_temp_dir("binary_files_inside_build_directories_are_removed");
        let target_exe = root.join("project").join("target").join("app.exe");
        write_file(&target_exe, b"1234");

        let config = ScanConfig::new(&[], ScanMode::All).unwrap();
        let results = collect_cleanup_targets_fast(&root, &config);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path(), root.join("project").join("target"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn files_only_mode_keeps_build_directory_context_for_nested_outputs() {
        let root =
            create_temp_dir("files_only_mode_keeps_build_directory_context_for_nested_outputs");
        let target_exe = root
            .join("project")
            .join("target")
            .join("bin")
            .join("app.exe");
        write_file(&target_exe, b"1234");

        let config = ScanConfig::new(&[], ScanMode::FilesOnly).unwrap();
        let results = collect_cleanup_targets_fast(&root, &config);

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].path(),
            root.join("project").join("target").join("bin")
        );
        assert_eq!(results[0].kind(), RemovalKind::FileGroup);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removable_files_in_same_directory_are_grouped() {
        let root = create_temp_dir("removable_files_in_same_directory_are_grouped");
        let build_dir = root.join("project").join("artifacts");
        write_file(&build_dir.join("a.obj"), b"12");
        write_file(&build_dir.join("b.obj"), b"123");
        write_file(&build_dir.join("c.pdb"), b"1");

        let config = ScanConfig::new(&[], ScanMode::FilesOnly).unwrap();
        let results = collect_cleanup_targets_fast(&root, &config);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path(), build_dir);
        assert_eq!(results[0].kind(), RemovalKind::FileGroup);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_mode_can_limit_results_to_directories() {
        let root = create_temp_dir("scan_mode_can_limit_results_to_directories");
        let target_dir = root.join("project").join("target");
        let output_file = root.join("project").join("app.exe");
        fs::create_dir_all(&target_dir).unwrap();
        fs::create_dir_all(output_file.parent().unwrap()).unwrap();
        write_file(&output_file, b"5678");

        let config = ScanConfig::new(&[], ScanMode::DirectoriesOnly).unwrap();
        let results = collect_cleanup_targets(&root, &config);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path(), target_dir);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn log_directory_only_counts_log_and_txt_files() {
        let root = create_temp_dir("log_directory_only_counts_log_and_txt_files");
        let logs_dir = root.join("logs");
        fs::create_dir_all(&logs_dir).unwrap();
        write_file(&logs_dir.join("app.log"), b"1234");
        write_file(&logs_dir.join("trace.txt"), b"12");
        write_file(&logs_dir.join("keep.json"), b"123456");

        let config = ScanConfig::new(&[], ScanMode::All).unwrap();
        let results = collect_cleanup_targets(&root, &config);
        let log_target = results
            .iter()
            .find(|entry| entry.path() == logs_dir)
            .expect("expected logs target");

        assert_eq!(log_target.kind(), RemovalKind::LogDirectory);
        assert_eq!(log_target.size(), 6);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scanning_inside_log_directory_returns_current_directory_as_target() {
        let root =
            create_temp_dir("scanning_inside_log_directory_returns_current_directory_as_target");
        let logs_dir = root.join("log");
        fs::create_dir_all(&logs_dir).unwrap();
        write_file(&logs_dir.join("app.log"), b"1234");
        write_file(&logs_dir.join("trace.log"), b"12");

        let config = ScanConfig::new(&[], ScanMode::All).unwrap();
        let results = collect_cleanup_targets_fast(&logs_dir, &config);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path(), logs_dir);
        assert_eq!(results[0].kind(), RemovalKind::LogDirectory);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn log_directory_cleanup_preserves_non_log_files() {
        let root = create_temp_dir("log_directory_cleanup_preserves_non_log_files");
        let logs_dir = root.join("logs");
        fs::create_dir_all(&logs_dir).unwrap();
        write_file(&logs_dir.join("app.log"), b"1234");
        write_file(&logs_dir.join("keep.json"), b"123456");

        cleanup_log_directory(&logs_dir).unwrap();

        assert!(!logs_dir.join("app.log").exists());
        assert!(logs_dir.join("keep.json").exists());
        assert!(logs_dir.exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn log_directory_cleanup_removes_folder_when_empty() {
        let root = create_temp_dir("log_directory_cleanup_removes_folder_when_empty");
        let logs_dir = root.join("logs");
        fs::create_dir_all(&logs_dir).unwrap();
        write_file(&logs_dir.join("app.log"), b"1234");
        write_file(&logs_dir.join("trace.txt"), b"12");

        cleanup_log_directory(&logs_dir).unwrap();

        assert!(!logs_dir.exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_target_expands_to_file_and_directory_operations() {
        let root = create_temp_dir("directory_target_expands_to_operations");
        let target_dir = root.join("target");
        let nested_dir = target_dir.join("debug");
        fs::create_dir_all(&nested_dir).unwrap();
        write_file(&nested_dir.join("app.exe"), b"1234");

        let target = RemovalTarget::new(target_dir.clone(), RemovalKind::Directory, 4);
        let operations = target.delete_operations();

        assert_eq!(
            operations.last().map(DeleteOperation::path),
            Some(target_dir.as_path())
        );
        assert!(operations.iter().any(|operation| {
            operation.path() == nested_dir.join("app.exe")
                && operation.action() == DeleteAction::File
        }));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn log_directory_target_only_expands_log_related_operations() {
        let root = create_temp_dir("log_directory_target_only_expands_log_related_operations");
        let logs_dir = root.join("logs");
        fs::create_dir_all(&logs_dir).unwrap();
        write_file(&logs_dir.join("app.log"), b"1234");
        write_file(&logs_dir.join("keep.json"), b"1234");

        let target = RemovalTarget::new(logs_dir.clone(), RemovalKind::LogDirectory, 4);
        let operations = target.delete_operations();

        assert!(operations.iter().any(|operation| {
            operation.path() == logs_dir.join("app.log") && operation.action() == DeleteAction::File
        }));
        assert!(
            !operations
                .iter()
                .any(|operation| operation.path() == logs_dir.join("keep.json"))
        );
        assert_eq!(
            operations.last().map(DeleteOperation::action),
            Some(DeleteAction::DirectoryIfEmpty)
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn log_directory_uses_directory_label() {
        let target = RemovalTarget::new(PathBuf::from("logs"), RemovalKind::LogDirectory, 0);
        assert_eq!(target.label(), "DIR ");
    }

    fn create_temp_dir(label: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("dust_{label}_{timestamp}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_file(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = File::create(path).unwrap();
        file.write_all(bytes).unwrap();
    }
}
