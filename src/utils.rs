use regex::Regex;
use rayon::prelude::*;
use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

/// 遞迴搜尋符合指定模式（如 bin、obj、node_modules）的資料夾。
///
/// 此函式會以廣度優先（BFS）方式遍歷指定路徑下所有子資料夾，
/// 並將所有符合 `pattern` 的資料夾路徑加入 `result` 向量中。
///
/// # 參數
/// - `path`: 根目錄的路徑參考
/// - `pattern`: 用於比對資料夾名稱的正規表示式
/// - `result`: 儲存符合條件的資料夾路徑清單
///
/// # 範例
/// ```rust
/// let mut result = Vec::new();
/// let pattern = Regex::new(r"(?i)([/\\])(bin|obj|node_modules)$").unwrap();
/// collect_matching_folders(Path::new("D:/Projects"), &pattern, &mut result);
/// ```
///
/// # 注意
/// - 只會收集資料夾（目錄），不處理檔案。
/// - 使用 BFS 實作，避免遞迴爆棧。
pub fn collect_matching_folders(path: &Path, pattern: &Regex, result: &mut Vec<PathBuf>) {
    let mut queue = VecDeque::new();
    queue.push_back(path.to_path_buf());

    while let Some(current) = queue.pop_front() {
        if let Ok(entries) = fs::read_dir(&current) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Some(path_str) = path.to_str() {
                        if pattern.is_match(path_str) {
                            result.push(path);
                            continue;
                        }
                    }
                    queue.push_back(path);
                }
            }
        }
    }
}

/// 計算單一資料夾的大小（以位元組為單位）
pub fn get_folder_size(path: &Path) -> Result<usize, std::io::Error> {
    let mut size = 0;
    let mut queue = VecDeque::new();
    queue.push_back(path.to_path_buf());

    while let Some(current) = queue.pop_front() {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() {
                if let Ok(metadata) = fs::metadata(&path) {
                    size += metadata.len() as usize;
                }
            } else if path.is_dir() {
                queue.push_back(path);
            }
        }
    }

    Ok(size)
}

/// 計算多個資料夾的總大小
pub fn calculate_folders_size(folders: &[PathBuf]) -> usize {
    let total = Arc::new(AtomicUsize::new(0));

    folders.par_iter().for_each(|folder| {
        if let Ok(size) = get_folder_size(folder) {
            total.fetch_add(size, Ordering::Relaxed);
        }
    });

    total.load(Ordering::Relaxed)
}

/// 將位元組格式化為人類可讀的單位（KB、MB、GB）
pub fn format_size(size: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * KB;
    const GB: usize = 1024 * MB;

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
