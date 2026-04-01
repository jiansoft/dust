# dust

[English](./README.md) | [繁體中文](./README.zh-TW.md)

`dust` 是一個 CLI 工具，可掃描指定目錄，並刪除常見的不應提交到版本控制的編譯產物、快取資料夾與產生檔案。

它適合用來快速清理包含 C#、Node.js、Rust 與 Zig 專案的大型工作目錄。

## 功能特色

- 遞迴掃描指定目錄
- 支援命令列傳入路徑或互動式輸入
- 支援全螢幕互動式 TUI 目錄瀏覽介面
- 刪除前會先列出所有符合的項目
- 執行刪除前會要求確認
- 支援 `--dry-run` 安全預覽
- 支援重複使用 `--exclude` glob 排除不想處理的路徑
- 內建略過常見中繼資料與保護資料夾以加快掃描並降低誤刪風險，例如 `.git`、`.idea`、`.vscode`、`coverage`、`deploy`，以及 `rustdoc` 產生的 API 文件輸出目錄
- 支援 `--yes` 以非互動方式執行
- 支援 `--dirs-only` 與 `--files-only` 掃描模式
- 支援 `--json` 輸出，方便腳本與 CI 使用
- 支援 `--quiet` 隱藏一般輸出
- 支援 `--no-progress` 停用刪除進度列
- 顯示真正的刪除進度條，包含百分比、目前目標與目前路徑摘要
- 支援 `--progress-style soft|minimal`
- 可清理多種語言常見的編譯資料夾與輸出檔案

## 支援的清理目標

### 資料夾

- `bin`
- `obj`
- `node_modules`
- `target`
- `zig-cache`
- `.zig-cache`
- `zig-out`
- `log`
- `logs`

### 檔案

- `*.pdb`
- `*.ilk`
- `*.o`
- `*.obj`
- `*.so`
- `*.a`
- `*.lib`
- `*.dll`
- `*.exe`
- `*.wasm`

像 `*.exe`、`*.dll`、`*.so`、`*.a`、`*.lib`、`*.wasm` 這類二進位產物，現在只會在已知的建置輸出目錄下清理，例如 `target`、`bin`、`obj`、`zig-out`、`zig-cache`、`.zig-cache`。

### `log` 與 `logs` 的特殊處理

- 只會刪除副檔名為 `.log` 或 `.txt` 的檔案
- 只有在資料夾內不再有任何檔案時，才會刪除 `log` 或 `logs` 資料夾本身
- 如果目前掃描的根目錄本身就叫做 `log` 或 `logs`，而且裡面有符合條件的檔案，這個目錄本身也會列入清理目標

## 安裝方式

### 前置需求

如果你的環境尚未安裝 Rust，請先完成安裝：

```bash
https://rustup.rs
```

### 從原始碼建置

```bash
git clone https://github.com/jiansoft/dust.git
cd dust
cargo build --release
```

### 加入 PATH

```bash
cp target/release/dust ~/.cargo/bin/
```

## Build Scripts

此專案內含兩個方便的建置腳本：

- [build.sh](./build.sh)：給 Unix、Linux、macOS 使用
- [build.bat](./build.bat)：給 Windows 使用

### 預設建置

Unix / macOS / Linux：

```bash
./build.sh
```

Windows：

```bat
build.bat
```

兩個腳本預設都會用 `release` 模式建置，並確認輸出執行檔確實存在。

### 指定不同的 profile

Unix / macOS / Linux：

```bash
PROFILE=debug ./build.sh
```

Windows：

```bat
set PROFILE=debug
build.bat
```

### 建置指定 target

Unix / macOS / Linux：

```bash
TARGETS="aarch64-unknown-linux-musl x86_64-unknown-linux-gnu" ./build.sh
```

Windows：

```bat
set TARGETS=aarch64-unknown-linux-musl x86_64-pc-windows-msvc
build.bat
```

當有設定 `TARGETS` 時，腳本會先執行 `rustup target add`，再依序建置各 target。

## 使用方式

### 掃描指定路徑

```bash
dust D:\Project\MyApp
```

### 使用互動式輸入

```bash
dust
```

未傳入路徑時，`dust` 會先要求輸入一個初始路徑，再開啟全螢幕互動式 TUI 目錄瀏覽介面。

- 在提示直接按 `Enter`，會使用目前工作目錄作為初始路徑
- 可輸入 Windows 路徑，例如 `D:\Project\MyApp`
- 也可輸入 Unix 路徑，例如 `/home/user/project`
- 也接受可解析成資料夾的相對路徑

- Windows：可從可用磁碟機、目前目錄、家目錄或上次選取的目錄開始
- Unix/macOS：可從 `/`、目前目錄、家目錄或上次選取的目錄開始
- 在 TUI 中可瀏覽資料夾、切換根路徑、查看預刪摘要、執行清理，或結束程式
- 在預設的互動式預覽中，`dust` 會優先顯示資料夾類目標；如果你想查看分組後的可刪除檔案目標，可改用 `--files-only`

每次清理完成後，TUI 會再次顯示，方便持續操作而不必重新啟動程式。

### 僅預覽，不刪除

```bash
dust . --dry-run
```

### 略過確認

```bash
dust . --yes
```

### 排除特定路徑

```bash
dust . --exclude '**/vendor/**' --exclude '**/third_party/**'
```

### 只清理資料夾

```bash
dust . --dirs-only
```

### 只清理檔案

```bash
dust . --files-only
```

### JSON 輸出

```bash
dust . --dry-run --json
```

### 安靜模式

```bash
dust . --yes --quiet
```

### 停用進度列

```bash
dust . --yes --no-progress
```

### 進度列風格

```bash
dust . --yes --progress-style soft
dust . --yes --progress-style minimal
```

`soft` 資訊較完整、存在感較高；`minimal` 會更輕、更安靜。

掃描完成後，`dust` 會列出預計刪除的資料夾與檔案；預設情況下，會在實際刪除前要求確認。

## 常見使用情境

- 在封存或分享前清理混合語言 monorepo
- 在檢查 git 狀態前先移除本機編譯產物
- 回收開發工作目錄中的磁碟空間

## 授權

MIT
