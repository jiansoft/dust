# dust

[English](./README.md) | [繁體中文](./README.zh-TW.md)

`dust` is a CLI tool that scans a directory and removes common build artifacts, cache folders, and generated binaries that should not be committed to version control.

It is designed for quickly cleaning large workspaces containing projects written in C#, Node.js, Rust, and Zig.

## Features

- Recursively scans a target directory
- Supports both command-line path input and interactive prompt input
- Shows all matched items before deletion
- Requires confirmation before removing anything
- Supports `--dry-run` for safe previewing
- Supports repeated `--exclude` glob filters to protect paths you do not want to touch
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

### Special handling for `log` and `logs`

- Only files ending in `.log` or `.txt` are removed
- The `log` or `logs` directory itself is removed only when no files remain inside it

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

## Usage

### Scan a specific path

```bash
dust D:\Project\MyApp
```

### Use interactive input

```bash
dust
```

When no path is provided, the program returns to the prompt after each cleanup run. Enter `q`, `quit`, `exit`, or submit an empty value to stop.

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
