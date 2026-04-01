# dust

[English](./README.md) | [繁體中文](./README.zh-TW.md)

`dust` is a CLI tool that scans a directory and removes common build artifacts, cache folders, and generated binaries that should not be committed to version control.

It is designed for quickly cleaning large workspaces containing projects written in C#, Node.js, Rust, and Zig.

## Features

- Recursively scans a target directory
- Supports both command-line path input and interactive prompt input
- Supports a full-screen interactive TUI directory browser
- Shows all matched items before deletion
- Requires confirmation before removing anything
- Supports `--dry-run` for safe previewing
- Supports repeated `--exclude` glob filters to protect paths you do not want to touch
- Skips common metadata and protected folders by default for faster and safer scans, including `.git`, `.idea`, `.vscode`, `coverage`, `deploy`, and `rustdoc` generated documentation output
- Supports `--yes` for non-interactive cleanup
- Supports `--dirs-only` and `--files-only` scan modes
- Supports `--json` output for scripts and CI
- Supports `--quiet` to suppress normal console output
- Supports `--no-progress` to disable the delete progress bar
- Shows a real delete progress bar with percentage, current target, and current path summary
- Supports `--progress-style soft|minimal`
- Cleans common build folders and generated files across multiple languages

## Supported Cleanup Targets

### Directories

- `bin`
- `obj`
- `node_modules`
- `target`
- `zig-cache`
- `.zig-cache`
- `zig-out`
- `log`
- `logs`

### Files

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

Generated binary files such as `*.exe`, `*.dll`, `*.so`, `*.a`, `*.lib`, and `*.wasm` are only removed when they are located under known build-output directories such as `target`, `bin`, `obj`, `zig-out`, `zig-cache`, or `.zig-cache`.

### Special handling for `log` and `logs`

- Only files ending in `.log` or `.txt` are removed
- The `log` or `logs` directory itself is removed only when no files remain inside it
- If the current scan root is itself named `log` or `logs`, it is also listed as a cleanup target when matching files are present

## Installation

### Prerequisites

Install Rust first if it is not already available on your machine:

```bash
https://rustup.rs
```

### Build from Source

```bash
git clone https://github.com/jiansoft/dust.git
cd dust
cargo build --release
```

### Add to PATH

```bash
cp target/release/dust ~/.cargo/bin/
```

## Build Scripts

This repository includes helper scripts for local builds:

- [build.sh](./build.sh) for Unix, Linux, and macOS
- [build.bat](./build.bat) for Windows

### Default build

Unix / macOS / Linux:

```bash
./build.sh
```

Windows:

```bat
build.bat
```

Both scripts build `dust` in `release` mode by default and verify that the output binary exists.

### Build with a different profile

Unix / macOS / Linux:

```bash
PROFILE=debug ./build.sh
```

Windows:

```bat
set PROFILE=debug
build.bat
```

### Build specific targets

Unix / macOS / Linux:

```bash
TARGETS="aarch64-unknown-linux-musl x86_64-unknown-linux-gnu" ./build.sh
```

Windows:

```bat
set TARGETS=aarch64-unknown-linux-musl x86_64-pc-windows-msvc
build.bat
```

When `TARGETS` is set, the scripts call `rustup target add` before building each target.

## Usage

### Scan a specific path

```bash
dust D:\Project\MyApp
```

### Use interactive input

```bash
dust
```

When no path is provided, `dust` first asks for an initial directory and then opens a full-screen interactive TUI browser.

- Press `Enter` on an empty prompt to start from the current working directory
- Enter a Windows path such as `D:\Project\MyApp` or a Unix path such as `/home/user/project`
- Relative paths are also accepted if they resolve to a directory

- Windows: you can start from available drives, the current directory, the home directory, or the last selected directory
- Unix/macOS: you can start from `/`, the current directory, the home directory, or the last selected directory
- Inside the TUI, browse directories, switch roots, preview planned deletions, clean, or quit
- In the default interactive preview, `dust` focuses on directory-like targets; use `--files-only` if you want the preview to list grouped removable files instead

After each cleanup run, the TUI is shown again so you can continue working without restarting the tool.

### Preview without deleting

```bash
dust . --dry-run
```

### Skip confirmation

```bash
dust . --yes
```

### Exclude paths

```bash
dust . --exclude '**/vendor/**' --exclude '**/third_party/**'
```

### Remove directories only

```bash
dust . --dirs-only
```

### Remove files only

```bash
dust . --files-only
```

### JSON output

```bash
dust . --dry-run --json
```

### Quiet mode

```bash
dust . --yes --quiet
```

### Disable progress bar

```bash
dust . --yes --no-progress
```

### Progress style

```bash
dust . --yes --progress-style soft
dust . --yes --progress-style minimal
```

`soft` is heavier and more descriptive. `minimal` is lighter and quieter.

After scanning, `dust` prints the folders and files that will be removed. By default it asks for confirmation before deletion.

## Typical Use Cases

- Clean mixed-language monorepos before archiving or sharing
- Remove local build artifacts before checking git status
- Reclaim disk space in development workspaces

## License

MIT
