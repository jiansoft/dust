# Releasing

This document defines the standard release process for `dust`.

## Versioning

- Use a `v` prefix for Git tags, for example `v0.2.0`
- Keep the package version in `Cargo.toml` aligned with the release tag
- Use the same value for the GitHub release title, for example `v0.2.0`

## Release Checklist

1. Update the package version in `Cargo.toml`
2. Review `README.md` and `README.zh-TW.md`
3. Run formatting, tests, and lint checks
4. Build at least one local release asset to validate packaging
5. Commit the release changes
6. Create the Git tag
7. Push the branch and the tag
8. Wait for GitHub Actions to build and publish release assets
9. Verify the GitHub Release page
10. Edit release notes if needed

## Local Validation

Run the standard checks before tagging:

```powershell
cargo fmt
cargo test
cargo clippy --all-targets --all-features
```

## Local Release Asset Build

On a Windows development machine, validate at least the host target:

```powershell
.\build-release-assets.ps1 -Targets x86_64-pc-windows-msvc
```

The script writes archives to `release-assets/`.

For Linux and macOS targets, `build-release-assets.ps1` uses `cargo zigbuild` with Zig.

Install the required tools before building those targets locally:

```powershell
cargo install --locked cargo-zigbuild
pip install ziglang
```

If you need a different target:

```powershell
.\build-release-assets.ps1 -Targets aarch64-pc-windows-msvc
```

## Commit and Tag

Example for `v0.2.0`:

```powershell
git add .
git commit -m "release: v0.2.0"
git tag v0.2.0
```

## Push

Push the branch first, then the tag:

```powershell
git push
git push origin v0.2.0
```

## GitHub Actions Release Flow

The repository includes:

- `.github/workflows/release-assets.yml`

When a tag matching `v*` is pushed, the workflow builds release assets for the configured target matrix and uploads them to the GitHub Release.

Current target matrix:

- `x86_64-pc-windows-msvc`
- `aarch64-pc-windows-msvc`
- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `aarch64-apple-darwin`

## How the Workflow Works

The release workflow is split into two stages:

1. `build`
2. `publish`

### `build` stage

The `build` job runs once per target in the matrix.

For each target, it:

- checks out the repository
- installs Zig and `cargo-zigbuild` for non-Windows targets
- runs `build-release-assets.ps1` for the current target
- produces one archive for that target
- uploads the archive as a workflow artifact

Windows targets use normal `cargo build`.

Linux and macOS targets use `cargo zigbuild` with Zig as the linker toolchain.

### `publish` stage

The `publish` job runs only when the workflow was triggered by a tag like `v0.2.0`.

It:

- waits for all matrix builds to finish
- downloads the workflow artifacts produced by the `build` stage
- uploads them to the GitHub Release
- asks GitHub to generate default release notes

### Manual workflow runs

If the workflow is started manually from the Actions tab, the `build` stage still runs and produces artifacts, but the `publish` stage does not run unless the workflow is running from a `v*` tag.

## Expected Release Asset Naming

Examples:

- `dust-v0.2.0-windows-x86_64.zip`
- `dust-v0.2.0-windows-aarch64.zip`
- `dust-v0.2.0-linux-x86_64.tar.gz`
- `dust-v0.2.0-linux-aarch64.tar.gz`
- `dust-v0.2.0-macos-aarch64.tar.gz`

## GitHub Release Fields

For a `v0.2.0` release:

- Tag: `v0.2.0`
- Release title: `v0.2.0`
- Release notes: use generated release notes, then edit manually if needed

If this is not the first release, select the previous tag when generating notes.

## Post-Release Verification

After GitHub Actions finishes:

- Confirm the release exists
- Confirm the tag and title are correct
- Confirm all expected assets are attached
- Confirm the release notes are readable
- Download at least one asset and verify the binary runs

## Standard Release Commands

Example for `v0.2.0`:

```powershell
cargo fmt
cargo test
cargo clippy --all-targets --all-features
.\build-release-assets.ps1 -Targets x86_64-pc-windows-msvc
git add .
git commit -m "release: v0.2.0"
git tag v0.2.0
git push
git push origin v0.2.0
```

## Notes

- On local Windows machines, do not assume every non-Windows target can be built successfully without extra toolchains or linkers
- The full multi-platform target matrix is primarily intended for GitHub-hosted runners
- `release-assets/` is ignored by git and should not be committed
