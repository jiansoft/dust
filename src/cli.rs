//! Command-line argument definitions for the `dust` binary.

use clap::{Parser, ValueEnum};

/// 清除常見編譯暫存資料夾與產物檔案
#[derive(Parser, Debug, Clone)]
#[command(name = "dust")]
#[command(
    about = "刪除 bin/obj/node_modules/target/zig-cache 等編譯產物的小工具",
    long_about = None
)]
#[command(
    after_help = "Examples:\n  dust D:\\Project\\MyApp\n  dust . --dry-run\n  dust . --exclude '**/vendor/**' --exclude '**/third_party/**'\n  dust . --dirs-only\n  dust . --files-only\n  dust . --yes"
)]
pub(crate) struct Cli {
    /// 要掃描的根目錄
    pub(crate) path: Option<String>,

    /// 只列出符合項目，不實際刪除
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// 略過刪除前確認
    #[arg(short = 'y', long)]
    pub(crate) yes: bool,

    /// 只清理資料夾
    #[arg(long, conflicts_with = "files_only")]
    pub(crate) dirs_only: bool,

    /// 只清理檔案
    #[arg(long, conflicts_with = "dirs_only")]
    pub(crate) files_only: bool,

    /// 以 glob 排除路徑，可重複使用
    #[arg(long, value_name = "GLOB")]
    pub(crate) exclude: Vec<String>,

    /// 隱藏一般輸出
    #[arg(long)]
    pub(crate) quiet: bool,

    /// 停用刪除進度列
    #[arg(long)]
    pub(crate) no_progress: bool,

    /// 進度列風格
    #[arg(long, value_enum, default_value_t = ProgressStyleKind::Soft)]
    pub(crate) progress_style: ProgressStyleKind,

    /// 輸出 JSON 結果
    #[arg(long)]
    pub(crate) json: bool,
}

/// Available visual styles for delete progress output.
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum ProgressStyleKind {
    /// Shows the full progress message with a visual separator.
    Soft,
    /// Shows a shorter progress message.
    Minimal,
}
