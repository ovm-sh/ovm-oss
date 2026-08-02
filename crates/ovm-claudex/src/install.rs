//! Managed CLIProxyAPI installs: download a release from GitHub, verify its
//! checksum, and install it under `~/.ovm/claudex/proxy/versions/<v>/` with
//! a `current` symlink — so the proxy is pinnable, updatable, and never a
//! brew dependency.

use crate::paths::ClaudexDirs;
use crate::{ClaudexError, Result};
use console::style;
use fs4::{FileExt, TryLockError};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const UPSTREAM_REPO: &str = "router-for-me/CLIProxyAPI";
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);
const MAX_ARCHIVE_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;
const MAX_CHECKSUM_DOWNLOAD_BYTES: u64 = 1024 * 1024;
const MAX_EXTRACTED_BINARY_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingUpdate {
    pub version: String,
    pub binary: PathBuf,
    /// Automatic updates obey `auto_update_proxy`; explicitly requested
    /// updates remain eligible for later safe activation even when it is off.
    #[serde(default)]
    pub automatic: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct UpdateCache {
    latest: String,
    checked_at: u64,
}

/// Held while checking, downloading, and publishing proxy update state.
/// The operating system releases it automatically on process exit.
pub struct UpdateLock {
    _file: File,
}

/// GitHub API base — env-overridable so tests can point at a local mock,
/// mirroring ovm core's `OVM_GITHUB_API_URL` convention.
fn github_api_base() -> String {
    std::env::var("OVM_GITHUB_API_URL").unwrap_or_else(|_| "https://api.github.com".into())
}

/// OVM registry base — env-overridable so tests can point at a local mock,
/// mirroring this crate's `OVM_CLAUDEX_*` override convention (and ovm core's
/// `OVM_REGISTRY_BASE_URL`). The registry is the verified source of truth; the
/// GitHub releases path is only the fail-open fallback.
fn registry_base() -> String {
    std::env::var("OVM_CLAUDEX_REGISTRY_URL").unwrap_or_else(|_| "https://ovm.sh/api".into())
}

/// The `cliproxyapi` `latest` from the OVM registry, or `None` on any failure
/// mode (unreachable, non-2xx, malformed JSON, no cliproxyapi entry). The
/// registry vouches only for versions the deep lane verified, so preferring it
/// keeps claudex on gated builds; `None` fails open to GitHub releases/latest.
fn registry_latest_version() -> Option<String> {
    let base = registry_base();
    let url = format!("{}/registry.json", base.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("ovm-claudex/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(5))
        .redirect(registry_redirect_policy())
        .build()
        .ok()?;
    let value: serde_json::Value = client
        .get(&url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .and_then(|response| response.json())
        .ok()?;
    let latest = value
        .get("products")?
        .as_array()?
        .iter()
        .find(|product| {
            product.get("product").and_then(serde_json::Value::as_str) == Some("cliproxyapi")
        })?
        .get("latest")?
        .as_str()?;
    let normalized = normalize_version(latest);
    validate_version(&normalized).ok()
}

/// Redirect rule for the OVM registry fetch: HTTPS only, and never off the host
/// we asked.
///
/// Deliberately NOT [`redirect_policy`], which allowlists GitHub's release-asset
/// hosts — pointing that at ovm.sh would let a registry response bounce the
/// fetch onto github.com and answer as the registry. This mirrors the semantics
/// of ovm core's `https_only_redirect_policy` (see
/// `crates/ovm/src/sources/registry.rs`), which the core registry client uses
/// for the same fetch: metadata must not cross hosts on redirect, because a
/// compromised or misconfigured host could otherwise hand us a forged `latest`.
fn registry_redirect_permitted(
    previous_host: Option<&str>,
    url: &reqwest::Url,
) -> std::result::Result<(), String> {
    if url.scheme() != "https" {
        return Err(format!(
            "refusing to follow a non-HTTPS registry redirect (scheme `{}`)",
            url.scheme()
        ));
    }
    let same_host = previous_host
        .zip(url.host_str())
        .is_some_and(|(previous, next)| {
            previous
                .trim_end_matches('.')
                .eq_ignore_ascii_case(next.trim_end_matches('.'))
        });
    if !same_host {
        return Err(format!(
            "refusing to follow a cross-host registry redirect (to `{}`)",
            url.host_str().unwrap_or("<no host>")
        ));
    }
    Ok(())
}

fn registry_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        let previous = attempt
            .previous()
            .last()
            .and_then(reqwest::Url::host_str)
            .map(str::to_owned);
        match registry_redirect_permitted(previous.as_deref(), attempt.url()) {
            Ok(()) => attempt.follow(),
            Err(message) => attempt.error(message),
        }
    })
}

fn github_download_base() -> Result<String> {
    let Ok(override_url) = std::env::var("OVM_CLAUDEX_DOWNLOAD_URL") else {
        return Ok(format!(
            "https://github.com/{UPSTREAM_REPO}/releases/download"
        ));
    };
    let url = reqwest::Url::parse(&override_url)
        .map_err(|error| ClaudexError::Message(format!("invalid test download URL: {error}")))?;
    // `Url::host_str()` keeps the brackets on IPv6 literals.
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if loopback && matches!(url.scheme(), "http" | "https") {
        Ok(override_url)
    } else {
        Err(ClaudexError::Message(
            "OVM_CLAUDEX_DOWNLOAD_URL is test-only and must use a loopback URL".into(),
        ))
    }
}

/// Hosts a GitHub fetch may redirect to: GitHub itself plus its release-asset
/// CDN hosts, mirroring ovm core's `GITHUB_DOWNLOAD_HOSTS`. The bare
/// `githubusercontent.com` apex is deliberately absent — it would also admit
/// user-controlled content hosts like `raw.githubusercontent.com`.
const REDIRECT_HOSTS: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// Refuse redirects that leave HTTPS or the GitHub host set. Loopback hosts
/// stay allowed (over plain HTTP too) only when the redirecting hop was also
/// loopback, so local test mirrors work without letting a public origin drive
/// requests into a service on the user's machine.
fn redirect_target_permitted(
    url: &reqwest::Url,
    allow_loopback: bool,
) -> std::result::Result<(), String> {
    let Some(host) = url.host_str() else {
        return Err("redirect URL has no host".into());
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    // `Url::host_str()` keeps the brackets on IPv6 literals.
    if allow_loopback && is_loopback_host(&host) {
        return Ok(());
    }
    if url.scheme() != "https" {
        return Err(format!(
            "refusing to follow a non-HTTPS redirect (scheme `{}`)",
            url.scheme()
        ));
    }
    let allowed = REDIRECT_HOSTS
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")));
    if allowed {
        Ok(())
    } else {
        Err(format!("redirect host `{host}` is not in the allowed set"))
    }
}

fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        let previous_is_loopback = attempt
            .previous()
            .last()
            .and_then(reqwest::Url::host_str)
            .map(is_loopback_host)
            .unwrap_or(false);
        match redirect_target_permitted(attempt.url(), previous_is_loopback) {
            Ok(()) => attempt.follow(),
            Err(message) => attempt.error(message),
        }
    })
}

fn http(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("ovm-claudex/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(5))
        .timeout(timeout)
        .redirect(redirect_policy())
        .build()
        .map_err(|error| ClaudexError::Message(format!("http client: {error}")))
}

fn read_limited(
    mut reader: impl Read,
    content_length: Option<u64>,
    limit: u64,
    description: &str,
) -> Result<Vec<u8>> {
    if content_length.is_some_and(|length| length > limit) {
        return Err(ClaudexError::Message(format!(
            "{description} exceeds the {limit}-byte limit"
        )));
    }

    let mut bytes = Vec::with_capacity(
        content_length
            .unwrap_or(0)
            .min(limit)
            .try_into()
            .unwrap_or(0),
    );
    reader
        .by_ref()
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| ClaudexError::Message(format!("{description} failed: {error}")))?;
    if bytes.len() as u64 > limit {
        return Err(ClaudexError::Message(format!(
            "{description} exceeds the {limit}-byte limit"
        )));
    }
    Ok(bytes)
}

pub fn acquire_update_lock(dirs: &ClaudexDirs) -> Result<UpdateLock> {
    let file = open_update_lock(dirs)?;
    match FileExt::try_lock(&file) {
        Ok(()) => return Ok(UpdateLock { _file: file }),
        Err(TryLockError::WouldBlock) => {
            eprintln!(
                "  {} Waiting for another OVM process to update the claudex proxy…",
                style("…").cyan()
            );
            FileExt::lock(&file)?;
        }
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    Ok(UpdateLock { _file: file })
}

pub fn try_acquire_update_lock(dirs: &ClaudexDirs) -> Result<Option<UpdateLock>> {
    let file = open_update_lock(dirs)?;
    match FileExt::try_lock(&file) {
        Ok(()) => Ok(Some(UpdateLock { _file: file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

fn open_update_lock(dirs: &ClaudexDirs) -> Result<File> {
    std::fs::create_dir_all(dirs.proxy_dir())?;
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dirs.proxy_update_lock())?)
}

/// `ovm claudex update [version]` — install (or switch to) a managed proxy.
pub fn update_command(version: Option<&str>) -> Result<()> {
    let dirs = ClaudexDirs::new()?;
    dirs.ensure_layout()?;

    let version = match version {
        Some(version) => validate_version(&normalize_version(version))?,
        None => validate_version(&latest_version()?)?,
    };

    let _lock = acquire_update_lock(&dirs)?;
    let target = dirs.proxy_versions_dir().join(&version).join("cliproxyapi");
    if target.is_file() {
        eprintln!(
            "  {} cliproxyapi {version} already installed.",
            style("✓").green()
        );
    }
    prepare_update_locked(&dirs, &version, false)?;

    if let Some(config) = crate::config::ClaudexConfig::load(&dirs.config_file())? {
        let session_guard = crate::proxy::SessionGuard::acquire(&dirs)?;
        if session_guard.is_exclusive() {
            let claude_home = dirs.claude_home();
            let invoked_inside_claudex = std::env::var_os("CLAUDE_CONFIG_DIR")
                .is_some_and(|path| std::path::Path::new(&path) == claude_home);
            crate::proxy::activate_pending_update(&dirs, &config, !invoked_inside_claudex)?;
        } else {
            eprintln!(
                "  {} cliproxyapi {} is verified and staged; it will activate after active claudex sessions exit.",
                style("…").cyan(),
                style(&version).green().bold()
            );
        }
    } else {
        let pending = load_pending_update(&dirs)?.ok_or_else(|| {
            ClaudexError::Message("prepared proxy update disappeared before activation".into())
        })?;
        switch_current(&dirs, &pending.binary)?;
        clear_pending_update(&dirs)?;
        eprintln!(
            "  {} cliproxyapi {} is now the managed proxy.",
            style("✓").green(),
            style(&version).green().bold()
        );
    }
    Ok(())
}

/// Install the newest upstream version and make it `current`. Used by setup
/// so a fresh machine never needs brew.
pub fn install_latest(dirs: &ClaudexDirs) -> Result<PathBuf> {
    let _lock = acquire_update_lock(dirs)?;
    let version = latest_version()?;
    let target = dirs.proxy_versions_dir().join(&version).join("cliproxyapi");
    if !target.is_file() {
        install(dirs, &version)?;
    }
    switch_current(dirs, &target)?;
    eprintln!(
        "  {} cliproxyapi {} installed (managed).",
        style("✓").green(),
        style(&version).green().bold()
    );
    Ok(target)
}

/// Newest version to install, e.g. "7.2.72". Consults the OVM registry first
/// (its `cliproxyapi` `latest` is the newest deep-lane-verified build) and
/// falls back to GitHub `releases/latest` when the registry is unreachable or
/// has no cliproxyapi entry — keeping the historical behaviour on failure.
pub fn latest_version() -> Result<String> {
    if let Some(version) = registry_latest_version() {
        return Ok(version);
    }
    let url = format!(
        "{}/repos/{UPSTREAM_REPO}/releases/latest",
        github_api_base()
    );
    let response: serde_json::Value = http(Duration::from_secs(10))?
        .get(&url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .and_then(|response| response.json())
        .map_err(|error| ClaudexError::Message(format!("release lookup failed: {error}")))?;
    let tag = response["tag_name"]
        .as_str()
        .ok_or_else(|| ClaudexError::Message("release has no tag_name".into()))?;
    Ok(normalize_version(tag))
}

/// Launch-time policy: discover and checksum-verify a newer managed proxy,
/// but leave activation to the session lock in `launch`. Network/update
/// failures are returned so the caller can warn and continue on the current
/// proxy.
pub fn maybe_prepare_auto_update(
    dirs: &ClaudexDirs,
    config: &crate::config::ClaudexConfig,
) -> Result<Option<PendingUpdate>> {
    if !config.auto_update_proxy || config.pin.is_some() {
        return Ok(None);
    }

    let Some(crate::proxy::ProxyBinary::Managed {
        version: current, ..
    }) = crate::proxy::resolve_binary(dirs, config)
    else {
        // Do not silently replace an explicitly system-managed proxy.
        return Ok(None);
    };

    let Some(_lock) = try_acquire_update_lock(dirs)? else {
        return Ok(None);
    };
    if load_pending_update(dirs)?.is_some_and(|pending| !pending.automatic) {
        return Ok(None);
    }
    let latest = latest_version_cached(dirs)?;
    if !is_newer(&latest, &current)? {
        return Ok(None);
    }

    let prepared = prepare_update_locked(dirs, &latest, true)?;
    eprintln!(
        "  {} Prepared cliproxyapi {} {} {} for safe activation.",
        style("↓").cyan(),
        style(&current).dim(),
        style("→").cyan(),
        style(&prepared.version).green().bold()
    );
    Ok(Some(prepared))
}

fn latest_version_cached(dirs: &ClaudexDirs) -> Result<String> {
    let now = now_unix();
    let interval = std::env::var("OVM_CLAUDEX_UPDATE_CHECK_INTERVAL_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(UPDATE_CHECK_INTERVAL);
    if let Ok(contents) = std::fs::read_to_string(dirs.proxy_update_cache()) {
        if let Ok(cache) = serde_json::from_str::<UpdateCache>(&contents) {
            if let Some(latest) = fresh_cached_version(cache, now, interval) {
                return Ok(latest);
            }
        }
    }

    let latest = validate_version(&latest_version()?)?;
    Version::parse(&latest).map_err(|error| {
        ClaudexError::Message(format!(
            "invalid upstream release version {latest:?}: {error}"
        ))
    })?;
    let cache = UpdateCache {
        latest: latest.clone(),
        checked_at: now,
    };
    let mut contents = serde_json::to_string_pretty(&cache)?;
    contents.push('\n');
    crate::config::write_atomic(&dirs.proxy_update_cache(), &contents, None)?;
    Ok(latest)
}

fn fresh_cached_version(cache: UpdateCache, now: u64, interval: Duration) -> Option<String> {
    (cache.checked_at <= now
        && now - cache.checked_at <= interval.as_secs()
        && Version::parse(&cache.latest).is_ok())
    .then_some(cache.latest)
}

fn is_newer(candidate: &str, current: &str) -> Result<bool> {
    let candidate = Version::parse(candidate).map_err(|error| {
        ClaudexError::Message(format!(
            "invalid candidate proxy version {candidate:?}: {error}"
        ))
    })?;
    let current = Version::parse(current).map_err(|error| {
        ClaudexError::Message(format!(
            "invalid current proxy version {current:?}: {error}"
        ))
    })?;
    Ok(candidate > current)
}

fn prepare_update_locked(
    dirs: &ClaudexDirs,
    version: &str,
    automatic: bool,
) -> Result<PendingUpdate> {
    let version = validate_version(&normalize_version(version))?;
    let target = dirs.proxy_versions_dir().join(&version).join("cliproxyapi");
    if !target.is_file() {
        install(dirs, &version)?;
    }
    let pending = PendingUpdate {
        version,
        binary: target,
        automatic,
    };
    let mut contents = serde_json::to_string_pretty(&pending)?;
    contents.push('\n');
    crate::config::write_atomic(&dirs.proxy_pending_update(), &contents, None)?;
    Ok(pending)
}

pub fn load_pending_update(dirs: &ClaudexDirs) -> Result<Option<PendingUpdate>> {
    match std::fs::read_to_string(dirs.proxy_pending_update()) {
        Ok(contents) => {
            let pending: PendingUpdate = serde_json::from_str(&contents)?;
            let version = validate_version(&pending.version)?;
            if version != pending.version {
                return Err(ClaudexError::Message(
                    "pending proxy update has a non-canonical version".into(),
                ));
            }
            Ok(Some(pending))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn clear_pending_update(dirs: &ClaudexDirs) -> Result<()> {
    match std::fs::remove_file(dirs.proxy_pending_update()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Download, checksum-verify, and unpack `version` into the versions dir.
pub fn install(dirs: &ClaudexDirs, version: &str) -> Result<PathBuf> {
    let asset = platform_asset(version)
        .ok_or_else(|| ClaudexError::Message("no CLIProxyAPI build for this platform".into()))?;
    let download_base = github_download_base()?;
    let base = format!("{}/v{version}", download_base.trim_end_matches('/'));

    eprintln!(
        "  {} Downloading cliproxyapi {version} ({asset})…",
        style("↓").cyan()
    );
    let client = http(Duration::from_secs(120))?;
    let archive_response = client
        .get(format!("{base}/{asset}"))
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|error| ClaudexError::Message(format!("download failed: {error}")))?;
    let archive_length = archive_response.content_length();
    let archive = read_limited(
        archive_response,
        archive_length,
        MAX_ARCHIVE_DOWNLOAD_BYTES,
        "release archive download",
    )?;

    let checksums_response = client
        .get(format!("{base}/checksums.txt"))
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|error| ClaudexError::Message(format!("checksums download failed: {error}")))?;
    let checksums_length = checksums_response.content_length();
    let checksums = read_limited(
        checksums_response,
        checksums_length,
        MAX_CHECKSUM_DOWNLOAD_BYTES,
        "checksums download",
    )?;
    let checksums = String::from_utf8(checksums).map_err(|error| {
        ClaudexError::Message(format!("checksums download is not valid UTF-8: {error}"))
    })?;
    let expected = match checksum_for(&checksums, &asset) {
        ChecksumEntry::Digest(digest) => digest,
        ChecksumEntry::NotListed => {
            return Err(ClaudexError::Message(format!(
                "checksums.txt has no entry for {asset}"
            )))
        }
        // Not "the digest disagrees" — we never got a digest. Saying so keeps
        // an unreadable manifest from reading as a tampered archive.
        ChecksumEntry::NotAManifest => {
            return Err(ClaudexError::Message(format!(
                "the checksums.txt served for cliproxyapi {version} contains no sha256 digest \
                 lines (truncated, or not a manifest) — refusing to install {asset}"
            )))
        }
        // Neither digest may be acted on: the manifest contradicts itself
        // about this asset, so it cannot say what the archive should be.
        ChecksumEntry::Conflicting(first, second) => {
            return Err(ClaudexError::Message(format!(
                "the checksums.txt served for cliproxyapi {version} records two different digests \
                 for {asset} ({first} and {second}) — refusing to install"
            )))
        }
    };
    let actual = hex(&sha2::Sha256::digest(&archive));
    if actual != expected {
        return Err(ClaudexError::Message(format!(
            "checksum mismatch for {asset}: expected {expected}, got {actual} — refusing to install"
        )));
    }

    let target_dir = dirs.proxy_versions_dir().join(version);
    std::fs::create_dir_all(&target_dir)?;
    let target = target_dir.join("cliproxyapi");
    extract_binary(&archive, &target)?;
    Ok(target)
}

/// Point the `current` symlink at `target` (atomically: replace, not edit).
pub(crate) fn switch_current(dirs: &ClaudexDirs, target: &std::path::Path) -> Result<()> {
    let current = dirs.proxy_current();
    let staging = current.with_file_name("current.new");
    let _ = std::fs::remove_file(&staging);
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, &staging)?;
    #[cfg(not(unix))]
    return Err(ClaudexError::Message(
        "managed installs are unix-only".into(),
    ));
    #[cfg(unix)]
    {
        std::fs::rename(&staging, &current)?;
        Ok(())
    }
}

/// Pull the proxy binary out of the release tarball. Upstream ships it as
/// `cli-proxy-api`; brew renames to `cliproxyapi` — accept both, install
/// under our canonical `cliproxyapi` name.
fn extract_binary(archive: &[u8], target: &std::path::Path) -> Result<()> {
    extract_binary_with_limit(archive, target, MAX_EXTRACTED_BINARY_BYTES)
}

fn extract_binary_with_limit(
    archive: &[u8],
    target: &std::path::Path,
    entry_limit: u64,
) -> Result<()> {
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        let is_proxy_binary = path
            .file_name()
            .map(|name| name == "cli-proxy-api" || name == "cliproxyapi")
            == Some(true);
        // Only a REGULAR file counts: a symlink/hardlink entry with the right
        // basename could otherwise plant a link we then chmod/execute.
        if is_proxy_binary && entry.header().entry_type() == tar::EntryType::Regular {
            if entry.size() > entry_limit {
                return Err(ClaudexError::Message(format!(
                    "extracted proxy binary exceeds the {entry_limit}-byte limit"
                )));
            }
            let staging = target.with_file_name("cliproxyapi.new");
            let _ = std::fs::remove_file(&staging);
            let write_result = (|| -> Result<()> {
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&staging)?;
                let written = std::io::copy(
                    &mut (&mut entry).take(entry_limit.saturating_add(1)),
                    &mut output,
                )?;
                if written > entry_limit {
                    return Err(ClaudexError::Message(format!(
                        "extracted proxy binary exceeds the {entry_limit}-byte limit"
                    )));
                }
                output.sync_all()?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))?;
                }
                std::fs::rename(&staging, target)?;
                Ok(())
            })();
            if write_result.is_err() {
                let _ = std::fs::remove_file(&staging);
            }
            write_result?;
            return Ok(());
        }
    }
    Err(ClaudexError::Message(
        "release archive does not contain a cliproxyapi binary".into(),
    ))
}

/// Release asset name for this platform, or None when upstream doesn't build
/// for it.
fn platform_asset(version: &str) -> Option<String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        _ => return None,
    };
    let arch = match std::env::consts::ARCH {
        // Upstream RENAMED the 64-bit ARM asset at 7.0.0: releases up to and
        // including 6.9.0 ship `arm64`, 7.0.0 and later ship `aarch64`. Pinning
        // either spelling alone 404s half the catalogue — and the download error
        // then reads as the release being broken rather than us asking for a
        // name that was never published.
        "aarch64" => arm64_asset_token(version),
        "x86_64" => "amd64",
        _ => return None,
    };
    Some(format!("CLIProxyAPI_{version}_{os}_{arch}.tar.gz"))
}

/// The 64-bit ARM token upstream published for this version.
///
/// CLIProxyAPI switched from goreleaser's `arm64` to `aarch64` at 7.0.0.
/// Anything unparseable is treated as new, since releases only move forward.
fn arm64_asset_token(version: &str) -> &'static str {
    let major: u32 = version
        .split('.')
        .next()
        .and_then(|part| part.parse().ok())
        .unwrap_or(u32::MAX);
    if major >= 7 {
        "aarch64"
    } else {
        "arm64"
    }
}

/// What a fetched `checksums.txt` says about one asset.
///
/// Three states, because two cannot hold them. With `Option<String>`, a body
/// that is not a manifest at all — a truncated transfer, a proxy error page
/// served with a 200 (a genuine 5xx is already refused by `error_for_status`) —
/// arrived as the same `None` as "this manifest simply lists other assets".
/// Worse, the old parser accepted *any* first token as a digest, so
/// `<html>  CLIProxyAPI_….tar.gz` would have been compared as a checksum and
/// reported as a mismatch: our own blindness rendered as a tampering verdict.
#[derive(Debug, PartialEq, Eq)]
enum ChecksumEntry {
    /// A well-formed `<64 hex>  <name>` line for this asset.
    Digest(String),
    /// The body is a digest manifest, and it lists no line for this asset.
    NotListed,
    /// The body holds no well-formed digest line at all, so it is not a
    /// manifest we understood. Must never be treated as a checksum.
    NotAManifest,
    /// Two well-formed lines record *different* digests for this asset. The
    /// manifest cannot be used for it: taking the first would let one appended
    /// line decide which digest the archive is held to.
    Conflicting(String, String),
}

fn is_sha256_hex(token: &str) -> bool {
    token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit())
}

/// Split a manifest line into `(digest-token, asset-name)`, stripping the `*`
/// that `sha256sum`'s binary mode prefixes the name with.
fn digest_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split_whitespace();
    let digest = parts.next()?;
    let name = parts.next()?.trim_start_matches('*');
    Some((digest, name))
}

/// goreleaser checksums.txt: `<sha256-hex>  <asset-name>` per line.
///
/// The digest shape is checked, not assumed, and duplicate lines are read to
/// the end rather than stopping at the first match — mirroring the `SHA256SUMS`
/// parsing in ovm core's Pi source. Duplicates that agree collapse into one
/// digest (the same statement twice); duplicates that disagree make the
/// manifest unusable for this asset.
fn checksum_for(checksums: &str, asset: &str) -> ChecksumEntry {
    let mut any_digest_line = false;
    let mut ours: Option<String> = None;

    for line in checksums.lines() {
        let Some((digest, name)) = digest_line(line) else {
            continue;
        };
        if !is_sha256_hex(digest) {
            continue;
        }
        any_digest_line = true;
        if name != asset {
            continue;
        }
        let digest = digest.to_ascii_lowercase();
        match &ours {
            Some(first) if *first != digest => {
                return ChecksumEntry::Conflicting(first.clone(), digest);
            }
            Some(_) => {}
            None => ours = Some(digest),
        }
    }

    match ours {
        Some(digest) => ChecksumEntry::Digest(digest),
        None if any_digest_line => ChecksumEntry::NotListed,
        None => ChecksumEntry::NotAManifest,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn normalize_version(version: &str) -> String {
    version.trim().trim_start_matches('v').to_string()
}

/// Versions become path components and URL segments — reject anything that
/// could traverse (`../x`) or smuggle separators.
fn validate_version(version: &str) -> Result<String> {
    let valid = !version.is_empty()
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && !version.split('.').any(|part| part.is_empty());
    if valid {
        Ok(version.to_string())
    } else {
        Err(ClaudexError::Message(format!(
            "invalid version string: {version:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_policy_pins_https_and_github_hosts() {
        let ok = |url: &str| redirect_target_permitted(&reqwest::Url::parse(url).unwrap(), true);

        assert!(ok("https://github.com/x").is_ok());
        assert!(ok("https://objects.githubusercontent.com/x").is_ok());
        assert!(ok("https://release-assets.githubusercontent.com/x").is_ok());
        // Loopback mocks may use plain HTTP.
        assert!(ok("http://127.0.0.1:8080/x").is_ok());
        assert!(ok("http://localhost:8080/x").is_ok());
        assert!(ok("http://[::1]:8080/x").is_ok());

        // HTTPS downgrade off loopback is refused.
        assert!(ok("http://github.com/x").is_err());
        // Hosts outside the pinned set are refused, including the
        // user-content apex that must not be admitted by suffix matching.
        assert!(ok("https://evil.example/x").is_err());
        assert!(ok("https://raw.githubusercontent.com/x").is_err());
        assert!(ok("https://github.com.evil.example/x").is_err());
    }

    #[test]
    fn redirect_to_loopback_requires_a_loopback_origin() {
        let loopback = reqwest::Url::parse("http://127.0.0.1:8080/asset").unwrap();

        assert!(redirect_target_permitted(&loopback, false).is_err());
        assert!(redirect_target_permitted(&loopback, true).is_ok());
    }

    #[test]
    fn limited_download_rejects_a_stream_larger_than_the_cap() {
        let error =
            read_limited(std::io::Cursor::new(b"12345"), None, 4, "release archive").unwrap_err();

        assert!(error.to_string().contains("exceeds the 4-byte limit"));
    }

    #[test]
    fn platform_asset_matches_upstream_naming() {
        let asset = platform_asset("7.2.72").expect("supported platform");
        assert!(asset.starts_with("CLIProxyAPI_7.2.72_"));
        assert!(asset.ends_with(".tar.gz"));
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        // 7.x uses aarch64; see arm64_asset_token_follows_the_upstream_rename_at_7_0_0.
        assert_eq!(asset, "CLIProxyAPI_7.2.72_darwin_aarch64.tar.gz");
    }

    /// The asset name must match what upstream actually publishes, at the
    /// version being installed.
    ///
    /// Two separate traps here. The original test asserted our own spelling
    /// back to us, so a wrong name passed review and CI. Then I "fixed" it to
    /// `arm64` after checking only 5.x and 6.x releases — which broke every
    /// current release, because upstream renamed the token to `aarch64` at
    /// 7.0.0. Pin both sides of that boundary against real published names.
    #[test]
    fn arm64_asset_token_follows_the_upstream_rename_at_7_0_0() {
        // Verified against real releases: 6.9.0 and 6.6.44 publish `arm64`,
        // 7.0.0, 7.2.72 and 7.2.113 publish `aarch64`.
        for version in ["5.0.0", "6.6.44", "6.9.0"] {
            assert_eq!(arm64_asset_token(version), "arm64", "{version}");
        }
        for version in ["7.0.0", "7.2.72", "7.2.113", "8.0.0"] {
            assert_eq!(arm64_asset_token(version), "aarch64", "{version}");
        }
        // Releases only move forward, so an unreadable version is treated as
        // new rather than silently falling back to the retired spelling.
        assert_eq!(arm64_asset_token("not-a-version"), "aarch64");
    }

    #[test]
    fn asset_names_use_a_published_arch_token() {
        for version in ["6.6.44", "7.2.113"] {
            let asset = platform_asset(version).expect("supported platform");
            assert!(
                asset.ends_with("_arm64.tar.gz")
                    || asset.ends_with("_aarch64.tar.gz")
                    || asset.ends_with("_amd64.tar.gz"),
                "unexpected arch token: {asset}"
            );
            assert!(
                asset.contains("_darwin_") || asset.contains("_linux_"),
                "unexpected OS token: {asset}"
            );
        }
    }

    #[test]
    fn checksum_parsing_finds_the_right_line() {
        let linux = "a".repeat(64);
        let darwin = "B".repeat(64);
        let text = format!(
            "{linux}  CLIProxyAPI_7.2.72_linux_amd64.tar.gz\n\
             {darwin}  CLIProxyAPI_7.2.72_darwin_aarch64.tar.gz\n"
        );
        assert_eq!(
            checksum_for(&text, "CLIProxyAPI_7.2.72_darwin_aarch64.tar.gz"),
            ChecksumEntry::Digest("b".repeat(64))
        );
        // A manifest that parsed fine and simply covers other assets.
        assert_eq!(
            checksum_for(&text, "missing.tar.gz"),
            ChecksumEntry::NotListed
        );
    }

    /// The digest's shape is checked, not assumed. The old parser took any
    /// first token on a matching line, so a 200-served error page or a
    /// truncated transfer became a "checksum" — compared, and then reported as
    /// a mismatch, i.e. as tampering. `sha256sum`'s binary-mode `*name` must
    /// still resolve to the same asset.
    #[test]
    fn a_body_that_is_not_a_manifest_is_not_a_checksum() {
        let asset = "CLIProxyAPI_7.2.72_darwin_aarch64.tar.gz";
        let digest = "c".repeat(64);

        assert_eq!(
            checksum_for(&format!("<html>  {asset}\n"), asset),
            ChecksumEntry::NotAManifest
        );
        assert_eq!(
            checksum_for("<html><body>503 Service Unavailable</body></html>", asset),
            ChecksumEntry::NotAManifest
        );
        assert_eq!(checksum_for("", asset), ChecksumEntry::NotAManifest);
        // Truncated mid-digest: not a digest line, so not a manifest.
        assert_eq!(
            checksum_for(&format!("{}  {asset}", "c".repeat(40)), asset),
            ChecksumEntry::NotAManifest
        );
        // Binary mode (`*name`) names the same asset.
        assert_eq!(
            checksum_for(&format!("{digest} *{asset}\n"), asset),
            ChecksumEntry::Digest(digest.clone())
        );
        // A well-formed manifest with one malformed line still parses; the
        // malformed line just isn't a digest for its asset. (Unlike ovm core's
        // Pi source, where "no digest for our asset" means installing on length
        // checks alone, `NotListed` is already a refusal here — see
        // `download_release` — so this asset still cannot install.)
        let mixed = format!("{digest}  other.tar.gz\nnot-a-digest  {asset}\n");
        assert_eq!(checksum_for(&mixed, asset), ChecksumEntry::NotListed);
    }

    /// Two valid lines that disagree about the same asset. The parser used to
    /// stop at the first match, so an appended line — or a manifest built from
    /// two concatenated releases — decided which digest the archive was held
    /// to. A self-contradicting manifest cannot verify anything, so it must
    /// refuse; duplicates that agree are merely redundant and still resolve.
    #[test]
    fn conflicting_duplicate_lines_cannot_verify_an_archive() {
        let asset = "CLIProxyAPI_7.2.72_darwin_aarch64.tar.gz";
        let first = "a".repeat(64);
        let second = "b".repeat(64);

        assert_eq!(
            checksum_for(&format!("{first}  {asset}\n{second}  {asset}\n"), asset),
            ChecksumEntry::Conflicting(first.clone(), second)
        );
        // Identical duplicates (case-insensitively — digests are compared
        // lowercased) say the same thing twice.
        assert_eq!(
            checksum_for(
                &format!(
                    "{first}  {asset}\n{}  *{asset}\n",
                    first.to_ascii_uppercase()
                ),
                asset
            ),
            ChecksumEntry::Digest(first)
        );
    }

    #[test]
    fn registry_redirects_stay_on_https_and_on_the_same_host() {
        let check = |previous: Option<&str>, url: &str| {
            registry_redirect_permitted(previous, &reqwest::Url::parse(url).unwrap())
        };

        assert!(check(Some("ovm.sh"), "https://ovm.sh/api/registry.json").is_ok());
        // A downgrade off HTTPS, even back to the same host.
        assert!(check(Some("ovm.sh"), "http://ovm.sh/api/registry.json").is_err());
        // The GitHub asset hosts `redirect_policy` allows are NOT the registry's
        // allowed set — that was the reason for a policy of its own.
        assert!(check(Some("ovm.sh"), "https://github.com/x").is_err());
        assert!(check(Some("ovm.sh"), "https://objects.githubusercontent.com/x").is_err());
        assert!(check(Some("ovm.sh"), "https://evil.example/registry.json").is_err());
        // Suffix confusion must not read as the same host.
        assert!(check(Some("ovm.sh"), "https://ovm.sh.evil.example/x").is_err());
        // No previous hop to compare against: refuse rather than guess.
        assert!(check(None, "https://ovm.sh/api/registry.json").is_err());
    }

    #[test]
    fn version_tags_are_normalized() {
        assert_eq!(normalize_version("v7.2.72"), "7.2.72");
        assert_eq!(normalize_version("7.2.72"), "7.2.72");
        assert_eq!(normalize_version(" v7.2.72\n"), "7.2.72");
    }

    #[test]
    fn semantic_version_order_drives_auto_update() {
        assert!(is_newer("7.2.74", "7.2.72").unwrap());
        assert!(!is_newer("7.2.74", "7.2.74").unwrap());
        assert!(!is_newer("7.2.74-rc.1", "7.2.74").unwrap());
        assert!(is_newer("8.0.0", "7.99.99").unwrap());
        assert!(is_newer("not-a-version", "7.2.72").is_err());
    }

    #[test]
    fn fresh_update_cache_avoids_a_network_lookup() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = ClaudexDirs::at(temp.path().to_path_buf());
        dirs.ensure_layout().unwrap();
        let cache = UpdateCache {
            latest: "7.2.74".into(),
            checked_at: now_unix(),
        };
        std::fs::write(
            dirs.proxy_update_cache(),
            serde_json::to_vec(&cache).unwrap(),
        )
        .unwrap();
        assert_eq!(latest_version_cached(&dirs).unwrap(), "7.2.74");
    }

    #[test]
    fn future_or_malformed_update_cache_is_never_fresh() {
        let now = now_unix();
        assert!(fresh_cached_version(
            UpdateCache {
                latest: "7.2.74".into(),
                checked_at: now + 60,
            },
            now,
            UPDATE_CHECK_INTERVAL,
        )
        .is_none());
        assert!(fresh_cached_version(
            UpdateCache {
                latest: "not-semver".into(),
                checked_at: now,
            },
            now,
            UPDATE_CHECK_INTERVAL,
        )
        .is_none());
    }

    #[test]
    fn pin_disables_launch_time_proxy_updates() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = ClaudexDirs::at(temp.path().to_path_buf());
        let config = crate::config::ClaudexConfig {
            pin: Some(crate::config::PinnedPair {
                claude: "2.1.207".into(),
                proxy: "7.2.72".into(),
            }),
            ..crate::config::ClaudexConfig::default()
        };
        assert!(maybe_prepare_auto_update(&dirs, &config).unwrap().is_none());
        assert!(!dirs.proxy_update_cache().exists());
    }

    #[test]
    #[cfg(unix)]
    fn automatic_check_never_overwrites_an_explicit_pending_version() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = ClaudexDirs::at(temp.path().to_path_buf());
        dirs.ensure_layout().unwrap();
        for version in ["7.2.70", "7.2.72"] {
            let binary = dirs.proxy_versions_dir().join(version).join("cliproxyapi");
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(binary, b"proxy").unwrap();
        }
        let current = dirs.proxy_versions_dir().join("7.2.72").join("cliproxyapi");
        std::os::unix::fs::symlink(current, dirs.proxy_current()).unwrap();

        prepare_update_locked(&dirs, "7.2.70", false).unwrap();
        assert!(
            maybe_prepare_auto_update(&dirs, &crate::config::ClaudexConfig::default())
                .unwrap()
                .is_none()
        );
        let pending = load_pending_update(&dirs).unwrap().unwrap();
        assert_eq!(pending.version, "7.2.70");
        assert!(!pending.automatic);
        assert!(!dirs.proxy_update_cache().exists());
    }

    #[test]
    #[cfg(unix)]
    fn launch_update_check_does_not_wait_for_another_updater() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = ClaudexDirs::at(temp.path().to_path_buf());
        dirs.ensure_layout().unwrap();
        let binary = dirs.proxy_versions_dir().join("7.2.72/cliproxyapi");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, b"proxy").unwrap();
        std::os::unix::fs::symlink(binary, dirs.proxy_current()).unwrap();
        let _held = acquire_update_lock(&dirs).unwrap();

        let started = std::time::Instant::now();
        assert!(
            maybe_prepare_auto_update(&dirs, &crate::config::ClaudexConfig::default())
                .unwrap()
                .is_none()
        );
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(!dirs.proxy_update_cache().exists());
    }

    #[test]
    fn extract_pulls_the_binary_out_of_a_tarball() {
        use std::io::Write;
        // Build a tiny gzipped tar with a decoy plus the binary.
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "README.md", &b"hello"[..])
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "cli-proxy-api", &b"BINX"[..])
            .unwrap();
        let tarball = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tarball).unwrap();
        let archive = gz.finish().unwrap();

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("cliproxyapi");
        extract_binary(&archive, &target).expect("extract");
        assert_eq!(std::fs::read(&target).unwrap(), b"BINX");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[test]
    fn extract_rejects_a_binary_larger_than_the_entry_cap() {
        use std::io::Write;

        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "cli-proxy-api", &b"12345"[..])
            .unwrap();
        let tarball = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tarball).unwrap();
        let archive = gz.finish().unwrap();

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("cliproxyapi");
        let error = extract_binary_with_limit(&archive, &target, 4).unwrap_err();

        assert!(error.to_string().contains("exceeds the 4-byte limit"));
        assert!(!target.exists());
        assert!(!temp.path().join("cliproxyapi.new").exists());
    }
}
