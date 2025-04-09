use clap::Parser;
use dialoguer::Input;
use regex::Regex;
use std::path::Path;
use std::fs;

/// 清除 bin、obj、node_modules 等編譯暫存資料夾
#[derive(Parser)]
#[command(name = "dust")]
#[command(about = "刪除 bin/obj/node_modules 資料夾的小工具", long_about = None)]
struct Cli {
    /// 要掃描的根目錄
    path: Option<String>,
}

fn main() {
    let cli = Cli::parse();

    // 若未提供參數，則互動式輸入
    let folder = cli.path.unwrap_or_else(|| Input::new()
        .with_prompt("請輸入要掃描刪除的根目錄")
        .interact_text()
        .unwrap());

    if !Path::new(&folder).exists() {
        eprintln!("❗ 路徑不存在: {}", folder);
        return;
    }

    println!("🔍 開始掃描目錄: {}", folder);

    let pattern = Regex::new(r"(?i)([/\\])(bin|obj|node_modules)$").unwrap();
    let mut to_delete = Vec::new();

    collect_matching_folders(Path::new(&folder), &pattern, &mut to_delete);

    if to_delete.is_empty() {
        println!("✅ 沒有找到要刪除的資料夾。");
        return;
    }

    for folder in &to_delete {
        match fs::remove_dir_all(folder) {
            Ok(_) => println!("✅ 已刪除: {}", folder),
            Err(err) => eprintln!("❌ 無法刪除 {}: {}", folder, err),
        }
    }

    println!("🚀 清除完成，共 {} 個資料夾。", to_delete.len());
}

fn collect_matching_folders(path: &Path, pattern: &Regex, result: &mut Vec<String>) {
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                if let Some(path_str) = entry_path.to_str() {
                    // 先往下遞迴，再判斷自己是不是要刪的
                    collect_matching_folders(&entry_path, pattern, result);

                    if pattern.is_match(path_str) {
                        result.push(path_str.to_string());
                    }
                }
            }
        }
    }
}