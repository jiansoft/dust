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
use walkdir::WalkDir;

/// 遞迴搜尋符合指定模式的資料夾（例如 `bin/`、`obj/`、`node_modules/`）
///
/// 此函式使用 [`walkdir`] 搭配 [`rayon`] 進行平行遍歷，可快速掃描大量檔案系統結構，
/// 將所有符合 `pattern` 的資料夾路徑加入 `result` 向量中。
///
/// # 參數
/// - `path`: 掃描的根目錄
/// - `pattern`: 用來比對資料夾路徑的正規表示式
/// - `result`: 儲存符合條件的資料夾清單（會被修改）
///
/// # 範例
/// ```
/// let mut folders = Vec::new();
/// let pattern = Regex::new(r"(?i)[/\\](bin|obj|node_modules)$").unwrap();
/// collect_matching_folders(Path::new("/my/project"), &pattern, &mut folders);
/// ```
///
/// # 注意事項
/// - 僅搜尋目錄（不包含檔案）
/// - 使用 rayon 平行處理，需將 result 設為 thread-safe 型別（目前假設使用外層保證）
/// - 本函式不會進行刪除，僅收集資料夾路徑
///
/// # 相依套件
/// - `walkdir`: 用於遞迴列出所有檔案與目錄
/// - `rayon`: 用於平行處理大量目錄
pub fn collect_matching_folders(path: &Path, pattern: &Regex, result: &mut Vec<PathBuf>) {
    let dirs: Vec<_> = WalkDir::new(path)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
        .collect();

    result.par_extend(
        dirs.par_iter()
            .filter(|entry| {
                entry.path()
                    .to_str()
                    .map_or(false, |s| pattern.is_match(s))
            })
            .map(|entry| entry.clone().into_path())
    );
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
