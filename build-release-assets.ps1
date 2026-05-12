[CmdletBinding()]
param(
    [string]$Profile = $(if ($env:PROFILE) { $env:PROFILE } else { "release" }),
    [string[]]$Targets = $(if ($env:TARGETS) { $env:TARGETS -split "\s+" | Where-Object { $_ } } else { @() }),
    [string]$OutputDir = $(if ($env:OUTPUT_DIR) { $env:OUTPUT_DIR } else { "release-assets" }),
    [switch]$SkipFetch,
    [switch]$SkipTargetAdd
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$BinName = "dust"
$RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$StartTime = Get-Date

if ($null -eq $Targets) {
    $Targets = @()
} elseif ($Targets -is [string]) {
    $Targets = @($Targets)
} else {
    $Targets = @($Targets)
}

if ($Profile -notin @("release", "debug")) {
    throw "Unsupported PROFILE: $Profile`nUse PROFILE=release or PROFILE=debug"
}

function Write-Step {
    param(
        [int]$Number,
        [int]$Total,
        [string]$Message
    )

    Write-Host ("[{0}/{1}] {2}" -f $Number, $Total, $Message)
}

function Get-CargoVersion {
    $cargoToml = Join-Path $RepoRoot "Cargo.toml"
    $versionLine = Select-String -Path $cargoToml -Pattern '^\s*version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $versionLine) {
        throw "Could not determine package version from Cargo.toml"
    }

    return $versionLine.Matches[0].Groups[1].Value
}

function Get-TargetMatrix {
    return @(
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "aarch64-apple-darwin"
    )
}

function Get-HostTriple {
    $hostLine = rustc -vV | Select-String '^host:\s+(.+)$' | Select-Object -First 1
    if (-not $hostLine) {
        throw "Could not determine Rust host target"
    }

    return $hostLine.Matches[0].Groups[1].Value
}

function Get-BinaryNameForTarget {
    param([string]$Target)

    if ($Target -like "*windows*") {
        return "$BinName.exe"
    }

    return $BinName
}

function Test-IsWindowsTarget {
    param([string]$Target)

    return $Target -like "*windows*"
}

function Test-RequiresZigbuild {
    param(
        [string]$Target,
        [string]$HostTriple
    )

    return (-not (Test-IsWindowsTarget -Target $Target)) -and ($Target -ne $HostTriple)
}

function Get-ArchiveExtension {
    param([string]$Target)

    if (Test-IsWindowsTarget -Target $Target) {
        return ".zip"
    }

    return ".tar.gz"
}

function Get-PlatformLabel {
    param([string]$Target)

    if (Test-IsWindowsTarget -Target $Target) { return "windows" }
    if ($Target -like "*linux*") { return "linux" }
    if ($Target -like "*darwin*") { return "macos" }
    return "unknown"
}

function Get-ArchLabel {
    param([string]$Target)

    if ($Target -like "x86_64-*") { return "x86_64" }
    if ($Target -like "aarch64-*") { return "aarch64" }
    return ($Target -split '-')[0]
}

function Get-OutputBinaryPath {
    param(
        [string]$Target,
        [string]$Profile
    )

    $binaryName = Get-BinaryNameForTarget -Target $Target
    return Join-Path $RepoRoot ("target/{0}/{1}/{2}" -f $Target, $Profile, $binaryName)
}

function Assert-ZigbuildToolchain {
    if (-not (Get-Command cargo-zigbuild -ErrorAction SilentlyContinue)) {
        throw @"
cargo-zigbuild is required for Linux and macOS release targets.
Install it with:
  cargo install --locked cargo-zigbuild
"@
    }

    if (-not (Get-Command zig -ErrorAction SilentlyContinue)) {
        throw @"
zig is required for Linux and macOS release targets.
Install it first, or install the Python zig package:
  pip install ziglang
"@
    }
}

function New-Archive {
    param(
        [string]$SourceDir,
        [string]$ArchivePath,
        [string]$Target
    )

    if (Test-Path -LiteralPath $ArchivePath) {
        Remove-Item -LiteralPath $ArchivePath -Force
    }

    if ((Get-ArchiveExtension -Target $Target) -eq ".zip") {
        Compress-Archive -Path (Join-Path $SourceDir "*") -DestinationPath $ArchivePath -Force
        return
    }

    $tarPath = Get-Command tar -ErrorAction SilentlyContinue
    if (-not $tarPath) {
        throw "tar is required to package non-Windows release assets"
    }

    $archiveName = Split-Path -Leaf $ArchivePath
    $parentDir = Split-Path -Parent $ArchivePath
    Push-Location $SourceDir
    try {
        & $tarPath.Path -czf (Join-Path $parentDir $archiveName) *
    } finally {
        Pop-Location
    }
}

Write-Step -Number 1 -Total 6 -Message "Checking Rust toolchain..."
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "cargo is not installed or not in PATH.`nInstall Rust first: https://rustup.rs"
}
if (-not (Get-Command rustc -ErrorAction SilentlyContinue)) {
    throw "rustc is not installed or not in PATH.`nInstall Rust first: https://rustup.rs"
}

Write-Step -Number 2 -Total 6 -Message "Tool versions:"
Write-Host ("  - {0}" -f (rustc --version))
Write-Host ("  - {0}" -f (cargo --version))

if (@($Targets).Count -eq 0) {
    $Targets = Get-TargetMatrix
}

$Version = Get-CargoVersion
$HostTriple = Get-HostTriple

if (-not $SkipFetch) {
    Write-Step -Number 3 -Total 6 -Message "Fetching dependencies..."
    Push-Location $RepoRoot
    try {
        cargo fetch
    } finally {
        Pop-Location
    }
} else {
    Write-Step -Number 3 -Total 6 -Message "Skipping dependency fetch."
}

$ResolvedOutputDir = Join-Path $RepoRoot $OutputDir
New-Item -ItemType Directory -Path $ResolvedOutputDir -Force | Out-Null

$archives = New-Object System.Collections.Generic.List[string]
$buildCount = 0
foreach ($target in $Targets) {
    $buildCount += 1
    Write-Host ""
    Write-Host ("[4/6] Build {0}: {1}" -f $buildCount, $target)

    if (-not $SkipTargetAdd) {
        rustup target add $target
    }

    $buildStart = Get-Date
    Push-Location $RepoRoot
    try {
        if (Test-RequiresZigbuild -Target $target -HostTriple $HostTriple) {
            Assert-ZigbuildToolchain
            cargo zigbuild --profile $Profile --target $target
        } else {
            cargo build --profile $Profile --target $target
        }
    } finally {
        Pop-Location
    }

    $outputBinary = Get-OutputBinaryPath -Target $target -Profile $Profile
    if (-not (Test-Path -LiteralPath $outputBinary)) {
        throw "Build finished, but binary was not found: $outputBinary"
    }

    $platform = Get-PlatformLabel -Target $target
    $arch = Get-ArchLabel -Target $target
    $archiveExtension = Get-ArchiveExtension -Target $target
    $archiveName = "$BinName-v$Version-$platform-$arch$archiveExtension"
    $stagingDir = Join-Path $ResolvedOutputDir $target

    if (Test-Path -LiteralPath $stagingDir) {
        Remove-Item -LiteralPath $stagingDir -Recurse -Force
    }
    New-Item -ItemType Directory -Path $stagingDir -Force | Out-Null

    Copy-Item -LiteralPath $outputBinary -Destination $stagingDir

    $pdbPath = [System.IO.Path]::ChangeExtension($outputBinary, ".pdb")
    if ((Test-IsWindowsTarget -Target $target) -and (Test-Path -LiteralPath $pdbPath)) {
        Copy-Item -LiteralPath $pdbPath -Destination $stagingDir
    }

    foreach ($doc in @("README.md", "README.zh-TW.md", "LICENSE")) {
        $docPath = Join-Path $RepoRoot $doc
        if (Test-Path -LiteralPath $docPath) {
            Copy-Item -LiteralPath $docPath -Destination $stagingDir
        }
    }

    $archivePath = Join-Path $ResolvedOutputDir $archiveName
    New-Archive -SourceDir $stagingDir -ArchivePath $archivePath -Target $target
    $archives.Add($archivePath)
    Remove-Item -LiteralPath $stagingDir -Recurse -Force

    $elapsed = (Get-Date) - $buildStart
    Write-Host ("Output binary: {0}" -f $outputBinary)
    Write-Host ("Release asset: {0}" -f $archivePath)
    Write-Host ("Elapsed: {0}" -f $elapsed.ToString("hh\:mm\:ss"))
}

Write-Host ""
Write-Step -Number 5 -Total 6 -Message "Generated release assets:"
foreach ($archive in $archives) {
    Write-Host ("  - {0}" -f $archive)
}

$totalElapsed = (Get-Date) - $StartTime
Write-Host ""
Write-Step -Number 6 -Total 6 -Message "Done."
Write-Host ("Targets built: {0}" -f $buildCount)
Write-Host ("Rust host target: {0}" -f $HostTriple)
Write-Host ("Total build time: {0}" -f $totalElapsed.ToString("hh\:mm\:ss"))
