# 🧹 dust

一個用來掃描並刪除目錄中常見的編譯暫存資料夾（`bin`、`obj`、`node_modules`）的 CLI 小工具。

支援：
- 提供資料夾路徑作為參數（如 `dust D:\Project\MyApp`）
- 若未指定路徑，將自動提示用戶輸入

## 🧰 功能特色

- 🔍 遞迴掃描資料夾
- 🧼 刪除 `bin` / `obj` / `node_modules` 資料夾
- 🤖 支援互動式輸入資料夾路徑
- 🚀 適合開發者快速清理大量專案暫存

## 📦 安裝方式

1. 安裝 Rust 工具鏈（如果尚未安裝）：
    ```bash
    https://rustup.rs
    ```

2. Clone 本專案並建置：
    ```bash
    git clone https://github.com/jiansoft/dust.git
    cd dust
    cargo build --release
    ```

3. 將可執行檔複製到 PATH：
    ```bash
    cp target/release/dust ~/.cargo/bin/
    ```

## 🚀 使用方式

```bash
# 傳入路徑參數
dust D:\Project\MyApp

# 或互動式輸入
dust
