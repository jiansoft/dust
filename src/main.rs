mod utils;

use clap::Parser;
use dialoguer::{Confirm, Input};
use rayon::prelude::*;
use regex::Regex;
use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use utils::{calculate_folders_size, collect_matching_folders, format_size};

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

    let folder = cli.path.unwrap_or_else(|| {
        Input::new()
            .with_prompt("請輸入要掃描刪除的根目錄")
            .interact_text()
            .unwrap()
    });

    if !Path::new(&folder).exists() {
        eprintln!("❗ 路徑不存在: {}", folder);
        return;
    }

    println!("🔍 開始掃描目錄: {}", folder);

    let mut to_delete = Vec::new();
    // 建立正規表示式，用來比對 bin、obj、node_modules 資料夾
    let pattern = Regex::new(r"(?i)(^|[/\\])(bin|obj|node_modules)$").unwrap();
    // 🕒 開始計時掃描
    let scan_start = Instant::now();

    collect_matching_folders(Path::new(&folder), &pattern, &mut to_delete);



    if to_delete.is_empty() {
        println!("✅ 沒有找到要刪除的資料夾。");
        return;
    }

    // 計算所有資料夾的大小
    let total_size = calculate_folders_size(&to_delete);
    let size_str = format_size(total_size as usize);
    let scan_elapsed = scan_start.elapsed();
    
    println!(
        "📋 預計刪除以下 {} 個資料夾，共 {}（掃描耗時：{:.2?}）：",
        to_delete.len(),
        size_str,
        scan_elapsed
    );

    for path in &to_delete {
        println!(" - {}", path.display());
    }

    // 確認刪除
    let confirmed = Confirm::new()
        .with_prompt("是否要刪除這些資料夾？")
        .default(false)
        .interact()
        .unwrap();

    if !confirmed {
        println!("❌ 已取消刪除操作。");
        return;
    }

    let success = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));

    let start_time = Instant::now();

    to_delete
        .par_iter()
        .for_each(|folder| match fs::remove_dir_all(folder) {
            Ok(_) => {
                println!("✅ 已刪除: {}", folder.display());
                success.fetch_add(1, Ordering::Relaxed);
            }
            Err(err) => {
                eprintln!("❌ 無法刪除 {}: {}", folder.display(), err);
                failed.fetch_add(1, Ordering::Relaxed);
            }
        });

    println!(
        "🚀 清除完成：成功 {} 個，失敗 {} 個。",
        success.load(Ordering::Relaxed),
        failed.load(Ordering::Relaxed)
    );

    println!("⏱️ 耗時：{:.2?}", start_time.elapsed());
}
