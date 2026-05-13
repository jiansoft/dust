//! Version update checks and self-update installation against GitHub releases.

use serde::Serialize;
use std::{
    cmp::Ordering,
    env,
    error::Error,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{self, Command},
    time::Duration,
};
use ureq::ResponseExt;

/// Version embedded from `Cargo.toml` at compile time.
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// GitHub API endpoint that returns the latest published release.
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/jiansoft/dust/releases/latest";
/// GitHub web endpoint that redirects to the latest release tag page.
const LATEST_RELEASE_REDIRECT_URL: &str = "https://github.com/jiansoft/dust/releases/latest";
/// Prefix for release tag pages.
const RELEASE_TAG_URL_PREFIX: &str = "https://github.com/jiansoft/dust/releases/tag/";
/// Raw project manifest used as a last-resort version source.
const RAW_CARGO_TOML_URL: &str = "https://raw.githubusercontent.com/jiansoft/dust/main/Cargo.toml";
/// Shared user agent for GitHub requests.
const USER_AGENT: &str = concat!("dust/", env!("CARGO_PKG_VERSION"));
/// Timeout used when the user explicitly asks for an update check.
const MANUAL_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
/// Short timeout used for best-effort startup checks.
const STARTUP_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// Timeout used when downloading a release archive for self-update.
const UPDATE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Minimal fields read from GitHub's latest-release response.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct GitHubRelease {
    /// Release tag name, usually in the form `v0.2.4`.
    tag_name: String,
    /// Browser URL for the release page.
    html_url: String,
    /// Downloadable archives attached to this release.
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

/// Minimal fields read from a GitHub release asset.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct GitHubAsset {
    /// Asset filename, for example `dust-v0.3.0-windows-x86_64.zip`.
    name: String,
    /// Direct browser-download URL for the asset.
    browser_download_url: String,
}

/// Cached GitHub release response and validator.
#[derive(Debug, Serialize, serde::Deserialize)]
struct UpdateCache {
    /// ETag returned by GitHub for conditional requests.
    etag: Option<String>,
    /// Last known release payload.
    release: GitHubRelease,
}

/// A newer release that should be shown to the user.
#[derive(Debug, Clone)]
pub(crate) struct UpdateNotice {
    /// Version of the currently running binary.
    pub(crate) current_version: String,
    /// Latest release version discovered on GitHub.
    pub(crate) latest_version: String,
    /// Browser URL where the release can be downloaded.
    pub(crate) release_url: String,
    /// Direct URL for the archive matching the running platform, when available.
    pub(crate) asset_download_url: Option<String>,
    /// Filename for the matching release archive, when available.
    pub(crate) asset_name: Option<String>,
}

/// Result returned after a replacement helper has been scheduled.
#[derive(Debug)]
pub(crate) struct UpdateInstall {
    /// Version that was downloaded.
    pub(crate) latest_version: String,
    /// Current executable that will be replaced.
    pub(crate) target_exe: PathBuf,
}

/// Progress emitted while installing an update.
#[derive(Debug, Clone)]
pub(crate) enum UpdateProgress {
    /// Preparing temporary paths and validating the selected asset.
    Preparing,
    /// Downloading the release archive.
    Downloading {
        /// Bytes written to disk so far.
        downloaded: u64,
        /// Total bytes reported by the server, when available.
        total: Option<u64>,
    },
    /// Extracting the downloaded archive.
    Extracting,
    /// Scheduling replacement of the running binary.
    Scheduling,
}

/// JSON payload emitted for update checks.
#[derive(Debug, Serialize)]
struct UpdateCheck {
    /// Version of the currently running binary.
    current_version: String,
    /// Latest release version discovered on GitHub.
    latest_version: String,
    /// Whether the latest release is newer than the running binary.
    update_available: bool,
    /// Browser URL where the release can be downloaded.
    release_url: String,
    /// Direct URL for the archive matching the running platform, when available.
    asset_download_url: Option<String>,
    /// Filename for the matching release archive, when available.
    asset_name: Option<String>,
    /// Human-readable status message.
    message: String,
}

/// Checks GitHub Releases for a newer version and prints the result.
pub(crate) fn run_check(json_mode: bool, quiet: bool) -> Result<(), Box<dyn Error>> {
    let payload = check_latest_release(MANUAL_CHECK_TIMEOUT)?;

    if json_mode {
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if !quiet {
        println!("{}", payload.message);
        if payload.update_available {
            println!("Download: {}", payload.release_url);
        }
    }

    Ok(())
}

/// Performs a quiet startup update check and prints only when a newer version exists.
pub(crate) fn notify_if_update_available(json_mode: bool, quiet: bool) {
    if let Some(notice) = startup_update_notice(json_mode, quiet) {
        eprintln!(
            "A newer dust version is available: v{}",
            notice.latest_version
        );
        eprintln!("Download: {}", notice.release_url);
    }
}

/// Performs a quiet startup check and returns a notice only when a newer version exists.
pub(crate) fn startup_update_notice(json_mode: bool, quiet: bool) -> Option<UpdateNotice> {
    if json_mode || quiet {
        return None;
    }

    let payload = check_latest_release(STARTUP_CHECK_TIMEOUT).ok()?;
    payload.update_available.then_some(UpdateNotice {
        current_version: payload.current_version,
        latest_version: payload.latest_version,
        release_url: payload.release_url,
        asset_download_url: payload.asset_download_url,
        asset_name: payload.asset_name,
    })
}

/// Downloads the matching release asset and reports progress before scheduling replacement.
pub(crate) fn install_update_with_progress<F>(
    notice: &UpdateNotice,
    mut progress: F,
) -> Result<UpdateInstall, Box<dyn Error>>
where
    F: FnMut(UpdateProgress),
{
    progress(UpdateProgress::Preparing);
    let asset_url = notice.asset_download_url.as_deref().ok_or_else(|| {
        format!(
            "No release asset matches this platform ({})",
            platform_asset_label()
        )
    })?;
    let asset_name = notice
        .asset_name
        .as_deref()
        .ok_or("The matching release asset did not include a filename")?;
    let current_exe = env::current_exe()?;
    let work_dir = update_work_dir(&notice.latest_version)?;
    let archive_path = work_dir.join(asset_name);
    let extract_dir = work_dir.join("extract");

    progress(UpdateProgress::Preparing);
    fs::create_dir_all(&work_dir)?;
    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir)?;
    }
    fs::create_dir_all(&extract_dir)?;

    download_release_asset(asset_url, &archive_path, &mut progress)?;
    progress(UpdateProgress::Extracting);
    extract_release_archive(&archive_path, &extract_dir)?;
    let new_exe = find_extracted_binary(&extract_dir)
        .ok_or_else(|| format!("Downloaded archive did not contain {}", binary_name()))?;
    progress(UpdateProgress::Scheduling);
    schedule_binary_replacement(&current_exe, &new_exe, &work_dir)?;

    Ok(UpdateInstall {
        latest_version: notice.latest_version.clone(),
        target_exe: current_exe,
    })
}

/// Fetches and converts the latest GitHub release into an update-check payload.
fn check_latest_release(timeout: Duration) -> Result<UpdateCheck, Box<dyn Error>> {
    let release = fetch_latest_release(timeout)?;
    Ok(build_update_check(release))
}

/// Builds an update-check payload from a release response.
fn build_update_check(release: GitHubRelease) -> UpdateCheck {
    let latest_version = normalize_version(&release.tag_name).unwrap_or(&release.tag_name);
    let update_available = is_newer_version(latest_version, CURRENT_VERSION);
    let message = if update_available {
        format!("A newer dust version is available: v{latest_version}")
    } else {
        format!("dust is up to date: v{CURRENT_VERSION}")
    };
    let matching_asset = matching_asset(&release, latest_version);
    let asset_download_url = matching_asset.map(|asset| asset.browser_download_url.clone());
    let asset_name = matching_asset.map(|asset| asset.name.clone());

    UpdateCheck {
        current_version: CURRENT_VERSION.to_string(),
        latest_version: latest_version.to_string(),
        update_available,
        release_url: release.html_url,
        asset_download_url,
        asset_name,
        message,
    }
}

/// Finds the release asset matching the running platform and architecture.
fn matching_asset<'a>(release: &'a GitHubRelease, version: &str) -> Option<&'a GitHubAsset> {
    let expected = expected_asset_name(version);
    release.assets.iter().find(|asset| asset.name == expected)
}

/// Fetches the latest release from GitHub using the provided timeout.
fn fetch_latest_release(timeout: Duration) -> Result<GitHubRelease, Box<dyn Error>> {
    fetch_latest_release_from_api(timeout)
        .or_else(|_| fetch_latest_release_from_redirect(timeout))
        .or_else(|_| fetch_latest_release_from_raw_manifest(timeout))
}

/// Fetches the latest release from the GitHub REST API.
fn fetch_latest_release_from_api(timeout: Duration) -> Result<GitHubRelease, Box<dyn Error>> {
    let cached = read_update_cache();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into();
    let mut request = agent
        .get(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", USER_AGENT);
    if let Some(etag) = cached
        .as_ref()
        .filter(|cache| !cache.release.assets.is_empty())
        .and_then(|cache| cache.etag.as_deref())
    {
        request = request.header("If-None-Match", etag);
    }

    let response = request.call()?;
    if response.status().as_u16() == 304 {
        return cached
            .map(|cache| cache.release)
            .ok_or_else(|| "GitHub returned 304 without a cached release".into());
    }
    if !response.status().is_success() {
        return Err(format!("GitHub release API returned {}", response.status()).into());
    }

    let etag = response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = response.into_body().read_to_string()?;
    let release: GitHubRelease = serde_json::from_str(&body)?;
    write_update_cache(UpdateCache {
        etag,
        release: release.clone(),
    });

    Ok(release)
}

/// Fetches the latest release through GitHub's public redirect endpoint.
fn fetch_latest_release_from_redirect(timeout: Duration) -> Result<GitHubRelease, Box<dyn Error>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build()
        .into();
    let response = agent
        .get(LATEST_RELEASE_REDIRECT_URL)
        .header("User-Agent", USER_AGENT)
        .call()?;
    let release_url = response.get_uri().to_string();
    let tag_name = tag_name_from_release_url(&release_url)
        .ok_or_else(|| format!("latest release redirect did not point to a tag: {release_url}"))?;

    Ok(GitHubRelease {
        tag_name: tag_name.to_string(),
        html_url: release_url,
        assets: Vec::new(),
    })
}

/// Fetches the version from the raw project manifest as a last-resort fallback.
fn fetch_latest_release_from_raw_manifest(
    timeout: Duration,
) -> Result<GitHubRelease, Box<dyn Error>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build()
        .into();
    let response = agent
        .get(RAW_CARGO_TOML_URL)
        .header("User-Agent", USER_AGENT)
        .call()?;
    let manifest = response.into_body().read_to_string()?;
    let version = package_version_from_manifest(&manifest)
        .ok_or("raw Cargo.toml did not contain a package version")?;
    Ok(GitHubRelease {
        tag_name: format!("v{version}"),
        html_url: format!("{RELEASE_TAG_URL_PREFIX}v{version}"),
        assets: Vec::new(),
    })
}

/// Returns a stable temp directory for this update attempt.
fn update_work_dir(version: &str) -> Result<PathBuf, Box<dyn Error>> {
    let mut version_slug = String::with_capacity(version.len());
    for ch in version.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            version_slug.push(ch);
        } else {
            version_slug.push('_');
        }
    }
    Ok(env::temp_dir().join(format!("dust-update-{version_slug}-{}", process::id())))
}

/// Downloads a release archive to disk.
fn download_release_asset<F>(
    url: &str,
    destination: &Path,
    progress: &mut F,
) -> Result<(), Box<dyn Error>>
where
    F: FnMut(UpdateProgress),
{
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(UPDATE_DOWNLOAD_TIMEOUT))
        .build()
        .into();
    let response = agent.get(url).header("User-Agent", USER_AGENT).call()?;
    if !response.status().is_success() {
        return Err(format!("Release asset download returned {}", response.status()).into());
    }

    let total = response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    let mut reader = response.into_body().into_reader();
    let mut file = fs::File::create(destination)?;
    let mut downloaded = 0;
    let mut buffer = [0_u8; 64 * 1024];
    progress(UpdateProgress::Downloading { downloaded, total });
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])?;
        downloaded += read as u64;
        progress(UpdateProgress::Downloading { downloaded, total });
    }
    Ok(())
}

/// Extracts the downloaded release archive using tools already available on each platform.
fn extract_release_archive(archive_path: &Path, extract_dir: &Path) -> Result<(), Box<dyn Error>> {
    #[cfg(windows)]
    let status = {
        let script_path = extract_dir.join("extract-update.ps1");
        fs::write(
            &script_path,
            r#"param(
    [Parameter(Mandatory=$true)][string]$ArchivePath,
    [Parameter(Mandatory=$true)][string]$DestinationPath
)
$ErrorActionPreference = 'Stop'
Expand-Archive -LiteralPath $ArchivePath -DestinationPath $DestinationPath -Force
"#,
        )?;
        Command::new("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(script_path)
            .arg(archive_path)
            .arg(extract_dir)
            .status()?
    };

    #[cfg(not(windows))]
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive_path)
        .arg("-C")
        .arg(extract_dir)
        .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("Archive extraction failed with status {status}").into())
    }
}

/// Finds the new `dust` binary inside the extracted archive.
fn find_extracted_binary(extract_dir: &Path) -> Option<PathBuf> {
    let direct = extract_dir.join(binary_name());
    if direct.is_file() {
        return Some(direct);
    }

    walkdir::WalkDir::new(extract_dir)
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.into_path())
        .find(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case(binary_name()))
        })
}

/// Schedules replacing the running executable after the current process exits.
fn schedule_binary_replacement(
    current_exe: &Path,
    new_exe: &Path,
    work_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    #[cfg(windows)]
    {
        let script_path = work_dir.join("install-update.ps1");
        fs::write(
            &script_path,
            r#"param(
    [Parameter(Mandatory=$true)][string]$TargetPath,
    [Parameter(Mandatory=$true)][string]$SourcePath,
    [Parameter(Mandatory=$true)][int]$PidToWait
)
$ErrorActionPreference = 'Stop'
$target = $TargetPath
$source = $SourcePath
$backup = "$target.old"
Wait-Process -Id $pidToWait -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 250
if (Test-Path -LiteralPath $backup) { Remove-Item -LiteralPath $backup -Force }
if (Test-Path -LiteralPath $target) { Move-Item -LiteralPath $target -Destination $backup -Force }
Move-Item -LiteralPath $source -Destination $target -Force
if (Test-Path -LiteralPath $backup) { Remove-Item -LiteralPath $backup -Force }
"#,
        )?;
        Command::new("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(script_path)
            .arg(current_exe)
            .arg(new_exe)
            .arg(process::id().to_string())
            .spawn()?;
    }

    #[cfg(not(windows))]
    {
        let script_path = work_dir.join("install-update.sh");
        fs::write(
            &script_path,
            r#"#!/bin/sh
set -eu
target="$1"
source="$2"
pid_to_wait="$3"
while kill -0 "$pid_to_wait" 2>/dev/null; do
  sleep 0.2
done
chmod +x "$source"
mv "$source" "$target"
"#,
        )?;
        Command::new("sh")
            .arg(script_path)
            .arg(current_exe)
            .arg(new_exe)
            .arg(process::id().to_string())
            .spawn()?;
    }

    Ok(())
}

/// Expected release-asset filename for this binary.
fn expected_asset_name(version: &str) -> String {
    format!(
        "dust-v{version}-{}-{}{}",
        platform_label(),
        arch_label(),
        archive_extension()
    )
}

/// Human-readable platform label used in errors.
fn platform_asset_label() -> String {
    format!("{}-{}", platform_label(), arch_label())
}

/// Binary filename in release archives.
fn binary_name() -> &'static str {
    if cfg!(windows) { "dust.exe" } else { "dust" }
}

fn archive_extension() -> &'static str {
    if cfg!(windows) { ".zip" } else { ".tar.gz" }
}

fn platform_label() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

fn arch_label() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "unknown"
    }
}

/// Reads the cached update response from disk.
fn read_update_cache() -> Option<UpdateCache> {
    let path = update_cache_path()?;
    let contents = fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Writes the update response cache, ignoring cache failures.
fn write_update_cache(cache: UpdateCache) {
    let Some(path) = update_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(contents) = serde_json::to_string(&cache) {
        let _ = fs::write(path, contents);
    }
}

/// Returns the platform-specific update cache file path.
fn update_cache_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        return Some(path.join("dust").join("update-cache.json"));
    }
    if let Some(path) = env::var_os("XDG_CACHE_HOME").map(PathBuf::from) {
        return Some(path.join("dust").join("update-cache.json"));
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| path.join(".cache").join("dust").join("update-cache.json"))
}

/// Extracts a tag name from a GitHub release tag URL.
fn tag_name_from_release_url(url: &str) -> Option<&str> {
    url.strip_prefix(RELEASE_TAG_URL_PREFIX)
        .and_then(|tag| tag.split(['?', '#']).next())
        .filter(|tag| !tag.is_empty())
}

/// Extracts the package version from a Cargo manifest.
fn package_version_from_manifest(manifest: &str) -> Option<&str> {
    let mut in_package = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("version") {
            let value = value.trim_start().strip_prefix('=')?.trim();
            return value.strip_prefix('"')?.split('"').next();
        }
    }
    None
}

/// Removes a leading `v`/`V` from a release tag.
fn normalize_version(version: &str) -> Option<&str> {
    version
        .strip_prefix('v')
        .or_else(|| version.strip_prefix('V'))
}

/// Returns whether `candidate` is newer than `current`.
fn is_newer_version(candidate: &str, current: &str) -> bool {
    compare_versions(candidate, current).is_gt()
}

/// Compares dotted numeric version strings.
fn compare_versions(left: &str, right: &str) -> Ordering {
    let left_parts = version_parts(left);
    let right_parts = version_parts(right);
    for index in 0..left_parts.len().max(right_parts.len()) {
        let left_part = left_parts.get(index).copied().unwrap_or(0);
        let right_part = right_parts.get(index).copied().unwrap_or(0);
        match left_part.cmp(&right_part) {
            Ordering::Equal => {}
            ordering => return ordering,
        }
    }
    Ordering::Equal
}

/// Parses the numeric prefix of each version component.
fn version_parts(version: &str) -> Vec<u64> {
    version
        .split(['.', '-'])
        .map(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .take_while(|digits| !digits.is_empty())
        .filter_map(|digits| digits.parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_patch_version_is_detected() {
        assert!(is_newer_version("0.2.5", "0.2.4"));
    }

    #[test]
    fn same_version_is_not_newer() {
        assert!(!is_newer_version("0.2.4", "0.2.4"));
    }

    #[test]
    fn v_prefix_is_ignored() {
        assert_eq!(normalize_version("v1.2.3"), Some("1.2.3"));
        assert_eq!(normalize_version("1.2.3"), None);
    }

    #[test]
    fn update_payload_reports_newer_release() {
        let payload = build_update_check(GitHubRelease {
            tag_name: "v999.0.0".to_string(),
            html_url: "https://example.test/release".to_string(),
            assets: Vec::new(),
        });

        assert!(payload.update_available);
        assert_eq!(payload.latest_version, "999.0.0");
    }

    #[test]
    fn update_payload_selects_matching_release_asset() {
        let expected_name = expected_asset_name("999.0.0");
        let payload = build_update_check(GitHubRelease {
            tag_name: "v999.0.0".to_string(),
            html_url: "https://example.test/release".to_string(),
            assets: vec![
                GitHubAsset {
                    name: "dust-v999.0.0-other-x86_64.zip".to_string(),
                    browser_download_url: "https://example.test/other".to_string(),
                },
                GitHubAsset {
                    name: expected_name.clone(),
                    browser_download_url: "https://example.test/match".to_string(),
                },
            ],
        });

        assert_eq!(payload.asset_name.as_deref(), Some(expected_name.as_str()));
        assert_eq!(
            payload.asset_download_url.as_deref(),
            Some("https://example.test/match")
        );
    }

    #[test]
    fn release_redirect_url_provides_tag_name() {
        assert_eq!(
            tag_name_from_release_url("https://github.com/jiansoft/dust/releases/tag/v0.3.0"),
            Some("v0.3.0")
        );
    }

    #[test]
    fn release_redirect_url_ignores_query_and_fragment() {
        assert_eq!(
            tag_name_from_release_url(
                "https://github.com/jiansoft/dust/releases/tag/v0.3.0?expanded=true#assets"
            ),
            Some("v0.3.0")
        );
    }

    #[test]
    fn package_version_is_read_from_manifest_package_section() {
        let manifest = r#"
[workspace]
resolver = "3"

[package]
name = "dust"
version = "0.3.0"

[dependencies]
version = "1"
"#;

        assert_eq!(package_version_from_manifest(manifest), Some("0.3.0"));
    }

    #[test]
    fn package_version_ignores_dependency_versions() {
        let manifest = r#"
[dependencies]
serde = { version = "1" }
"#;

        assert_eq!(package_version_from_manifest(manifest), None);
    }
}
