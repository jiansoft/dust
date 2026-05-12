//! Version update checks against the project's GitHub releases.

use serde::Serialize;
use std::{cmp::Ordering, env, error::Error, fs, path::PathBuf, time::Duration};
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

/// Minimal fields read from GitHub's latest-release response.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct GitHubRelease {
    /// Release tag name, usually in the form `v0.2.4`.
    tag_name: String,
    /// Browser URL for the release page.
    html_url: String,
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

    UpdateCheck {
        current_version: CURRENT_VERSION.to_string(),
        latest_version: latest_version.to_string(),
        update_available,
        release_url: release.html_url,
        message,
    }
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
    if let Some(etag) = cached.as_ref().and_then(|cache| cache.etag.as_deref()) {
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
    })
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
        });

        assert!(payload.update_available);
        assert_eq!(payload.latest_version, "999.0.0");
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
