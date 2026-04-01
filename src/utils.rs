use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Serialize;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};
#[cfg(test)]
use std::{fs, io};
use walkdir::{IntoIter, WalkDir};

const DIRECTORY_NAMES: &[&str] = &[
    "bin",
    "obj",
    "node_modules",
    "target",
    "zig-cache",
    ".zig-cache",
    "zig-out",
];

const LOG_DIRECTORY_NAMES: &[&str] = &["log", "logs"];

const FILE_EXTENSIONS: &[&str] = &[
    "pdb", "ilk", "o", "obj", "so", "a", "lib", "dll", "exe", "wasm",
];

const LOG_FILE_EXTENSIONS: &[&str] = &["log", "txt"];

/// The kind of filesystem entry scheduled for removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalKind {
    /// A directory and all of its contents.
    Directory,
    /// A log directory that only deletes `.log` and `.txt` files, then removes
    /// the directory if it becomes empty.
    LogDirectory,
    /// A single file.
    File,
}

/// A single filesystem deletion action used to build progress-aware cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteAction {
    /// Delete a file.
    DeleteFile,
    /// Delete a directory and treat failure as an error.
    DeleteDirectory,
    /// Delete a directory only if it is empty; otherwise skip it.
    DeleteDirectoryIfEmpty,
}

/// A concrete deletion step derived from a cleanup target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteOperation {
    path: PathBuf,
    action: DeleteAction,
}

impl DeleteOperation {
    /// Creates a new deletion operation.
    pub fn new(path: PathBuf, action: DeleteAction) -> Self {
        Self { path, action }
    }

    /// Returns the filesystem path for this deletion operation.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the action to perform.
    pub fn action(&self) -> DeleteAction {
        self.action
    }
}

/// Selection mode for cleanup targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    /// Include both directories and files.
    All,
    /// Include directories only.
    DirectoriesOnly,
    /// Include files only.
    FilesOnly,
}

/// Configures how cleanup targets are collected.
#[derive(Debug, Clone)]
pub struct ScanConfig {
    exclude_set: GlobSet,
    mode: ScanMode,
}

impl ScanConfig {
    /// Builds a scan configuration from glob exclusion patterns and a scan mode.
    pub fn new(exclusions: &[String], mode: ScanMode) -> Result<Self, globset::Error> {
        let mut builder = GlobSetBuilder::new();

        for exclusion in exclusions {
            builder.add(Glob::new(exclusion)?);
        }

        Ok(Self {
            exclude_set: builder.build()?,
            mode,
        })
    }

    /// Returns the configured scan mode.
    pub fn mode(&self) -> ScanMode {
        self.mode
    }

    fn is_excluded(&self, path: &Path) -> bool {
        self.exclude_set.is_match(path)
    }
}

/// A filesystem entry scheduled for removal.
///
/// `dust` stores the entry kind, path, and precomputed size so the scan phase
/// can present a preview without recalculating the total later.
#[derive(Debug, Clone, Serialize)]
pub struct RemovalTarget {
    path: PathBuf,
    kind: RemovalKind,
    size: u64,
}

impl RemovalTarget {
    /// Creates a new cleanup target.
    pub fn new(path: PathBuf, kind: RemovalKind, size: u64) -> Self {
        Self { path, kind, size }
    }

    /// Returns the filesystem path of this cleanup target.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the kind of this cleanup target.
    pub fn kind(&self) -> RemovalKind {
        self.kind
    }

    /// Returns the precomputed size of this cleanup target in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Returns a short label used in CLI output.
    pub fn label(&self) -> &'static str {
        match self.kind {
            RemovalKind::Directory => "DIR ",
            RemovalKind::LogDirectory => "LOG ",
            RemovalKind::File => "FILE",
        }
    }

    /// Expands this target into concrete deletion operations.
    pub fn delete_operations(&self) -> Vec<DeleteOperation> {
        match self.kind {
            RemovalKind::Directory => collect_directory_delete_operations(&self.path),
            RemovalKind::LogDirectory => collect_log_directory_delete_operations(&self.path),
            RemovalKind::File => vec![DeleteOperation::new(
                self.path.clone(),
                DeleteAction::DeleteFile,
            )],
        }
    }
}

/// Recursively scans a directory and returns all matching cleanup targets.
///
/// The scanner applies built-in rules for supported build directories and file
/// extensions, optional exclusion globs, and a scan mode that can limit results
/// to directories or files only.
///
/// When a directory matches, it is added once and its descendants are skipped
/// so the scan avoids duplicate matches and unnecessary traversal.
pub fn collect_cleanup_targets(root: &Path, config: &ScanConfig) -> Vec<RemovalTarget> {
    let mut targets = Vec::new();
    let mut iter = WalkDir::new(root).min_depth(1).into_iter();

    while let Some(next) = iter.next() {
        let entry = match next {
            Ok(entry) => entry,
            Err(_) => continue,
        };

        let path = entry.path();

        if config.is_excluded(path) {
            if entry.file_type().is_dir() {
                iter.skip_current_dir();
            }
            continue;
        }

        if entry.file_type().is_dir() && should_remove_dir(path) {
            if matches!(config.mode(), ScanMode::All | ScanMode::DirectoriesOnly) {
                targets.push(RemovalTarget::new(
                    path.to_path_buf(),
                    RemovalKind::Directory,
                    directory_size(path),
                ));
            }
            iter.skip_current_dir();
            continue;
        }

        if entry.file_type().is_dir() && should_cleanup_log_dir(path) {
            if matches!(config.mode(), ScanMode::All | ScanMode::DirectoriesOnly) {
                let size = log_directory_cleanup_size(path);
                if size > 0 || directory_has_no_files(path) {
                    targets.push(RemovalTarget::new(
                        path.to_path_buf(),
                        RemovalKind::LogDirectory,
                        size,
                    ));
                }
            }
            iter.skip_current_dir();
            continue;
        }

        if entry.file_type().is_file()
            && should_remove_file(path)
            && matches!(config.mode(), ScanMode::All | ScanMode::FilesOnly)
        {
            targets.push(RemovalTarget::new(
                path.to_path_buf(),
                RemovalKind::File,
                file_size(path),
            ));
        }
    }

    targets
}

/// Returns the total size of multiple cleanup targets in bytes.
pub fn calculate_entries_size(entries: &[RemovalTarget]) -> u64 {
    entries.iter().map(RemovalTarget::size).sum()
}

/// Formats a byte count into a human-readable string.
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

fn should_remove_dir(path: &Path) -> bool {
    matches_name(path.file_name(), DIRECTORY_NAMES)
}

fn should_cleanup_log_dir(path: &Path) -> bool {
    matches_name(path.file_name(), LOG_DIRECTORY_NAMES)
}

fn should_remove_file(path: &Path) -> bool {
    path.extension().and_then(OsStr::to_str).is_some_and(|ext| {
        FILE_EXTENSIONS
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(ext))
    })
}

fn should_remove_log_file(path: &Path) -> bool {
    path.extension().and_then(OsStr::to_str).is_some_and(|ext| {
        LOG_FILE_EXTENSIONS
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(ext))
    })
}

fn matches_name(name: Option<&OsStr>, candidates: &[&str]) -> bool {
    name.and_then(OsStr::to_str).is_some_and(|value| {
        candidates
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(value))
    })
}

fn file_size(path: &Path) -> u64 {
    path.metadata().map(|metadata| metadata.len()).unwrap_or(0)
}

fn directory_size(path: &Path) -> u64 {
    let iter: IntoIter = WalkDir::new(path).into_iter();
    iter.filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .sum()
}

fn log_directory_cleanup_size(path: &Path) -> u64 {
    let iter: IntoIter = WalkDir::new(path).into_iter();
    iter.filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|entry_path| should_remove_log_file(entry_path))
        .map(|entry_path| file_size(&entry_path))
        .sum()
}

fn directory_has_no_files(path: &Path) -> bool {
    let iter: IntoIter = WalkDir::new(path).min_depth(1).into_iter();
    !iter
        .filter_map(|entry| entry.ok())
        .any(|entry| entry.file_type().is_file())
}

#[cfg(test)]
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
fn directory_is_empty(path: &Path) -> io::Result<bool> {
    Ok(fs::read_dir(path)?.next().is_none())
}

fn collect_directory_delete_operations(path: &Path) -> Vec<DeleteOperation> {
    let iter: IntoIter = WalkDir::new(path).contents_first(true).into_iter();
    iter.filter_map(|entry| entry.ok())
        .map(|entry| {
            let action = if entry.file_type().is_dir() {
                DeleteAction::DeleteDirectory
            } else {
                DeleteAction::DeleteFile
            };
            DeleteOperation::new(entry.into_path(), action)
        })
        .collect()
}

fn collect_log_directory_delete_operations(path: &Path) -> Vec<DeleteOperation> {
    let iter: IntoIter = WalkDir::new(path).contents_first(true).into_iter();
    iter.filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let entry_path = entry.into_path();

            if entry_path.is_file() && should_remove_log_file(&entry_path) {
                return Some(DeleteOperation::new(entry_path, DeleteAction::DeleteFile));
            }

            if entry_path.is_dir() {
                return Some(DeleteOperation::new(
                    entry_path,
                    DeleteAction::DeleteDirectoryIfEmpty,
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
        let output_file = root.join("project").join("app.exe");
        fs::create_dir_all(&target_dir).unwrap();
        fs::create_dir_all(output_file.parent().unwrap()).unwrap();
        write_file(&target_dir.join("artifact.bin"), b"1234");
        write_file(&output_file, b"5678");

        let config = ScanConfig::new(&[], ScanMode::All).unwrap();
        let results = collect_cleanup_targets(&root, &config);

        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|entry| entry.path() == target_dir));
        assert!(results.iter().any(|entry| entry.path() == output_file));

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
        let nested_dir = target_dir.join("nested");
        fs::create_dir_all(&nested_dir).unwrap();
        write_file(&nested_dir.join("artifact.bin"), b"1234");

        let target = RemovalTarget::new(target_dir.clone(), RemovalKind::Directory, 4);
        let operations = target.delete_operations();

        assert_eq!(operations.len(), 3);
        assert!(
            operations
                .iter()
                .any(|op| op.path() == nested_dir.join("artifact.bin")
                    && op.action() == DeleteAction::DeleteFile)
        );
        assert!(
            operations
                .iter()
                .any(|op| op.path() == nested_dir && op.action() == DeleteAction::DeleteDirectory)
        );
        assert!(
            operations
                .iter()
                .any(|op| op.path() == target_dir && op.action() == DeleteAction::DeleteDirectory)
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn log_directory_target_only_expands_log_related_operations() {
        let root = create_temp_dir("log_directory_target_only_expands_log_related_operations");
        let logs_dir = root.join("logs");
        let nested_dir = logs_dir.join("archive");
        fs::create_dir_all(&nested_dir).unwrap();
        write_file(&logs_dir.join("app.log"), b"1234");
        write_file(&logs_dir.join("keep.json"), b"1234");
        write_file(&nested_dir.join("trace.txt"), b"12");

        let target = RemovalTarget::new(logs_dir.clone(), RemovalKind::LogDirectory, 6);
        let operations = target.delete_operations();

        assert!(
            operations
                .iter()
                .any(|op| op.path() == logs_dir.join("app.log")
                    && op.action() == DeleteAction::DeleteFile)
        );
        assert!(
            operations
                .iter()
                .any(|op| op.path() == nested_dir.join("trace.txt")
                    && op.action() == DeleteAction::DeleteFile)
        );
        assert!(
            !operations
                .iter()
                .any(|op| op.path() == logs_dir.join("keep.json"))
        );
        assert!(operations.iter().any(
            |op| op.path() == nested_dir && op.action() == DeleteAction::DeleteDirectoryIfEmpty
        ));
        assert!(
            operations
                .iter()
                .any(|op| op.path() == logs_dir
                    && op.action() == DeleteAction::DeleteDirectoryIfEmpty)
        );

        fs::remove_dir_all(root).unwrap();
    }

    fn create_temp_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("dust_{prefix}_{unique}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(path: &Path, contents: &[u8]) {
        let mut file = File::create(path).unwrap();
        file.write_all(contents).unwrap();
    }
}
