use super::{latest_beta_newer_than_stable, SelfUpdateChannel};
use crate::bundle_manifest::{BundleManifest, BUNDLE_MANIFEST_NAME};
use crate::error::{OvmError, Result};
use crate::self_manager::SelfManager;
use crate::sources::{
    download_http_client, http_client, validate_download_url, GITHUB_DOWNLOAD_HOSTS,
};
use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tar::Archive;

const DEFAULT_GITHUB_API: &str = "https://api.github.com";
const CONTROL_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_PROBE_RETRY: Duration = Duration::from_millis(25);
const RELEASE_REPOSITORY: &str = "ovm-sh/ovm-oss";
/// api.github.com host: the entry point for token-authenticated asset downloads
/// (`/repos/<slug>/releases/assets/<id>`). Only added to the allowed download
/// hosts when a token is present; the redirect target CDN hosts are unchanged.
const GITHUB_ASSET_API_HOST: &str = "api.github.com";

#[derive(Debug, Clone, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubAsset {
    #[serde(default)]
    id: u64,
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedRelease {
    pub version: String,
    archive_name: String,
    archive_url: String,
    checksum_url: String,
    /// Whether a plain-HTTP loopback asset URL is acceptable for this release.
    /// True only when the metadata `api_base` is itself loopback — which happens
    /// solely under the `OVM_GITHUB_API_URL` test override. In production the api
    /// base is `api.github.com`, so a metadata-supplied loopback asset URL is
    /// refused (SSRF guard).
    allow_loopback: bool,
}

pub struct PreparedBundle {
    pub release: ResolvedRelease,
    pub manifest: BundleManifest,
    pub source_dir: PathBuf,
    _temp: tempfile::TempDir,
}

pub(crate) struct BlockedTerminationSignals {
    previous: libc::sigset_t,
}

impl BlockedTerminationSignals {
    pub(crate) fn new() -> Result<Self> {
        // Blocking termination signals keeps the in-memory rollback snapshot alive
        // until activation either commits or restores. Pending signals are delivered
        // when the guard drops and the previous mask is restored.
        unsafe {
            let mut blocked = std::mem::zeroed();
            libc::sigemptyset(&mut blocked);
            libc::sigaddset(&mut blocked, libc::SIGINT);
            libc::sigaddset(&mut blocked, libc::SIGTERM);
            // A dropped terminal/SSH session mid-activation is the likely
            // signal here; the rollback snapshot lives only in memory.
            libc::sigaddset(&mut blocked, libc::SIGHUP);
            let mut previous = std::mem::zeroed();
            let result = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous);
            if result != 0 {
                return Err(std::io::Error::from_raw_os_error(result).into());
            }
            Ok(Self { previous })
        }
    }
}

impl Drop for BlockedTerminationSignals {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateDecision {
    /// The resolved release matches the running version; nothing to do.
    AlreadyLatest,
    /// The resolved release is older than the running version; refuse.
    Downgrade,
    /// The resolved release is newer; install it.
    Proceed,
}

/// Prerelease-aware update gate. `semver::Version` orders prereleases below
/// their release (`0.2.0-alpha.1 < 0.2.0`) and above the previous release
/// (`0.1.0 < 0.2.0-alpha.1`), so this correctly proceeds on alpha upgrades and
/// refuses an alpha → trailing-stable move.
fn update_decision(current: &Version, target: &Version) -> UpdateDecision {
    match target.cmp(current) {
        std::cmp::Ordering::Equal => UpdateDecision::AlreadyLatest,
        std::cmp::Ordering::Less => UpdateDecision::Downgrade,
        std::cmp::Ordering::Greater => UpdateDecision::Proceed,
    }
}

/// Resolve the baseline for the no-op/downgrade gate of `ovm self update`.
///
/// Returns the `(current, target)` versions to compare, or `None` when the
/// update must simply proceed: a dev snapshot is selected (unordered against
/// releases, so an explicit update installs the requested release), or either
/// version is unparseable. When nothing is selected yet the compiled version is
/// the baseline. The gate is driven by the SELECTED version so a dev-snapshot
/// user requesting the stable equal to the compiled version is never told
/// "already latest".
fn selection_gate(
    selected: Option<&str>,
    compiled: &str,
    target: &str,
) -> Option<(Version, Version)> {
    if selected.is_some_and(|version| version.starts_with("dev-")) {
        return None;
    }
    let baseline = selected.unwrap_or(compiled);
    match (Version::parse(baseline), Version::parse(target)) {
        (Ok(current), Ok(target)) => Some((current, target)),
        _ => None,
    }
}

fn downgrade_message(current: &Version, target: &Version) -> String {
    let hint = if current.pre.is_empty() {
        ""
    } else {
        " The alpha channel is ahead of stable here; wait for stable to catch up or stay on alpha."
    };
    format!(
        "Refusing to downgrade OVM {current} -> {target}; use `ovm self use` to switch to an installed version.{hint}"
    )
}

pub fn update(channel: SelfUpdateChannel, dry_run: bool) -> Result<()> {
    let manager = SelfManager::new()?;
    update_with(
        &manager,
        channel,
        &github_api_base(),
        github_token().as_deref(),
        dry_run,
    )
}

/// Resolve the optional GitHub API token used to authenticate release metadata
/// and asset downloads.
///
/// Only `OVM_GITHUB_TOKEN` is honored — an ambient `GITHUB_TOKEN` inherited from
/// the surrounding shell or CI is deliberately NOT used, so a token that happens
/// to be present in the environment is never silently attached to OVM's requests.
/// Empty/whitespace-only values are treated as absent. A token lets `ovm self
/// update` reach a private release repository (e.g. testing the alpha channel
/// before the repo is public) and raises the api.github.com rate limit; callers
/// that need auth must set `OVM_GITHUB_TOKEN` explicitly. The token is only ever
/// sent as an `Authorization: Bearer` header to api.github.com and is never
/// logged or embedded in a URL.
fn github_token() -> Option<String> {
    let value = std::env::var("OVM_GITHUB_TOKEN").ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Testable core of [`update`]: the caller injects the target [`SelfManager`]
/// and GitHub API base so an integration test can drive the full
/// download → verify → install → activate → probe → rollback flow against a
/// throwaway `~/.ovm` and a mock release server.
fn update_with(
    manager: &SelfManager,
    channel: SelfUpdateChannel,
    api_base: &str,
    token: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        let version = resolve_release(channel, api_base, token)?.version;
        println!("release {version}");
        println!("install {}", manager.version_dir(&version).display());
        println!("refresh {}", manager.control_plane_path().display());
        println!("activate {version}");
        return Ok(());
    }

    let bundle = prepare_from_api(channel, api_base, token)?;
    // Never silently replace OVM with an older release: a stale or replayed
    // "latest" (or a backport cut after a newer minor) must not downgrade
    // every updater. Equal is a no-op; older is an explicit error. The
    // comparison is prerelease-aware, so switching from an alpha back to
    // stable while stable still trails (e.g. current 0.2.0-alpha.3, target
    // stable 0.1.0) refuses rather than downgrading.
    //
    // The baseline is the SELECTED version (`ovm self current`), NOT the
    // compile-time constant: a user on a dev snapshot requesting the stable
    // that happens to equal the control plane's compiled version must not get
    // a spurious AlreadyLatest. A dev snapshot is unordered against releases,
    // so an explicit update from one skips the gate and installs the request.
    let selected = manager.current_version()?;
    if let Some((current, target)) = selection_gate(
        selected.as_deref(),
        env!("CARGO_PKG_VERSION"),
        &bundle.release.version,
    ) {
        match update_decision(&current, &target) {
            UpdateDecision::AlreadyLatest => {
                eprintln!(
                    "  {} OVM {current} is already the latest release",
                    console::style("✓").green()
                );
                return Ok(());
            }
            UpdateDecision::Downgrade => {
                return Err(OvmError::Message(downgrade_message(&current, &target)));
            }
            UpdateDecision::Proceed => {}
        }
    }
    let operation = manager.acquire_operation_lock()?;
    manager.install_bundle(
        &bundle.release.version,
        &bundle.manifest,
        &bundle.source_dir,
    )?;
    let result = activate_release(manager, &bundle.release.version);
    drop(operation);
    result?;
    eprintln!(
        "  {} OVM {} is installed and active",
        console::style("✓").green(),
        console::style(&bundle.release.version).bold()
    );
    Ok(())
}

/// Atomically activate an already-installed self version: snapshot the current
/// selection and control plane, refresh + switch + probe, and roll everything
/// back if the probe fails. The caller must already hold the self operation
/// lock. Termination signals are blocked across the swap so a dropped session
/// can't strand the in-memory rollback snapshot.
pub(crate) fn activate_release(manager: &SelfManager, version: &str) -> Result<()> {
    let selection = manager.snapshot_selection(version)?;
    let control = manager.snapshot_control_plane()?;
    let control_backup = manager.snapshot_control_backup()?;
    let signals = BlockedTerminationSignals::new()?;
    let activation = (|| {
        manager.refresh_control_plane(version)?;
        manager.use_version(version)?;
        probe_control_plane(manager, version)
    })();
    let result = match activation {
        Ok(()) => manager.prune_inactive_dev_versions().map(|_| ()),
        Err(error) => {
            let selection_recovery = manager.restore_selection(&selection);
            let control_recovery = manager.restore_path(&control);
            let backup_recovery = manager.restore_path(&control_backup);
            Err(update_failure(
                error,
                combine_recovery(
                    selection_recovery,
                    combine_recovery(control_recovery, backup_recovery),
                ),
            ))
        }
    };
    drop(signals);
    result
}

/// Download, verify, and install the channel's latest release WITHOUT
/// activating it — the launch-time `on` policy stages here and activates at the
/// next invocation. Returns the staged version, or `None` when the release is
/// not newer than the active self version (stale cache, already current, or a
/// dev snapshot is selected). The download runs before any lock is taken; only
/// the immutable install briefly holds the self operation lock.
pub(crate) fn stage_latest(
    manager: &SelfManager,
    channel: SelfUpdateChannel,
) -> Result<Option<String>> {
    stage_latest_with(
        manager,
        channel,
        &github_api_base(),
        github_token().as_deref(),
    )
}

fn stage_latest_with(
    manager: &SelfManager,
    channel: SelfUpdateChannel,
    api_base: &str,
    token: Option<&str>,
) -> Result<Option<String>> {
    let Some(current) = manager.current_version()? else {
        return Ok(None);
    };
    // A dev snapshot is developer-controlled; never stage over it.
    if current.starts_with("dev-") {
        return Ok(None);
    }
    let bundle = prepare_from_api(channel, api_base, token)?;
    // Re-check against the ACTIVE version (not the compile-time constant): the
    // release must be strictly newer, or staging is a no-op.
    if let (Ok(current), Ok(target)) = (
        Version::parse(&current),
        Version::parse(&bundle.release.version),
    ) {
        if !matches!(update_decision(&current, &target), UpdateDecision::Proceed) {
            return Ok(None);
        }
    }
    let operation = manager.acquire_operation_lock()?;
    let installed = manager.install_bundle(
        &bundle.release.version,
        &bundle.manifest,
        &bundle.source_dir,
    );
    drop(operation);
    installed?;
    Ok(Some(bundle.release.version))
}

/// The channel's latest release version string (honoring the
/// `OVM_SELF_UPDATE_*_VERSION` test overrides). Used to populate the cached
/// self-latest check consulted by the launch path.
pub(crate) fn resolved_latest_version(channel: SelfUpdateChannel) -> Result<String> {
    release_version(channel)
}

fn prepare_from_api(
    channel: SelfUpdateChannel,
    api_base: &str,
    token: Option<&str>,
) -> Result<PreparedBundle> {
    let release = resolve_release(channel, api_base, token)?;
    let temp = tempfile::Builder::new()
        .prefix("ovm-self-update-")
        .tempdir()?;
    let archive = temp.path().join(&release.archive_name);
    let checksum = temp.path().join(format!("{}.sha256", release.archive_name));

    download(
        &release.archive_url,
        &archive,
        token,
        release.allow_loopback,
    )?;
    download(
        &release.checksum_url,
        &checksum,
        token,
        release.allow_loopback,
    )?;
    verify_checksum(&archive, &checksum, &release.archive_name)?;

    let source_dir = temp.path().join("bundle");
    let manifest = extract_bundle(&archive, &source_dir)?;
    Ok(PreparedBundle {
        release,
        manifest,
        source_dir,
        _temp: temp,
    })
}

fn combine_recovery(first: Result<()>, second: Result<()>) -> Result<()> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first), Err(second)) => Err(OvmError::Message(format!(
            "selection recovery failed: {first}; control-plane recovery failed: {second}"
        ))),
    }
}

fn update_failure(error: OvmError, recovery: Result<()>) -> OvmError {
    match recovery {
        Ok(()) => error,
        // Both the update and the rollback to the prior version failed, so the
        // control plane may be half-switched. Point the user at the manual
        // escape hatches instead of leaving them with a bare error: `ovm self
        // repair` restores the previous control plane, and reinstalling OVM
        // rebuilds it from scratch.
        Err(recovery_error) => OvmError::Message(format!(
            "{error}; automatic recovery also failed: {recovery_error}. \
             Run `ovm self repair` to restore the previous control plane, or reinstall OVM \
             from https://ovm.sh to recover."
        )),
    }
}

fn probe_control_plane(manager: &SelfManager, expected_version: &str) -> Result<()> {
    probe_control_plane_with_timeout(manager, expected_version, control_probe_timeout())
}

fn probe_control_plane_with_timeout(
    manager: &SelfManager,
    expected_version: &str,
    timeout: Duration,
) -> Result<()> {
    let stdout = tempfile::NamedTempFile::new()?;
    let stderr = tempfile::NamedTempFile::new()?;
    let mut child = std::process::Command::new(manager.control_plane_path())
        .args(["self", "current"])
        .env_remove(crate::self_manager::SELF_CHILD_ENV)
        // The probe runs the freshly activated control plane; it must not try to
        // activate a pending self-update itself (it would deadlock on the self
        // operation lock this activation already holds).
        .env(
            crate::commands::self_autoupdate::SKIP_SELF_AUTOUPDATE_ENV,
            "1",
        )
        .stdout(Stdio::from(stdout.reopen()?))
        .stderr(Stdio::from(stderr.reopen()?))
        .spawn()
        .map_err(|error| {
            OvmError::Message(format!("updated OVM control plane did not start: {error}"))
        })?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if started.elapsed() < timeout => {
                std::thread::sleep(CONTROL_PROBE_RETRY);
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(OvmError::Message(format!(
                    "updated OVM control plane activation probe timed out after {} milliseconds",
                    timeout.as_millis()
                )));
            }
        }
    };
    let stdout = std::fs::read_to_string(stdout.path())?;
    let stderr = std::fs::read_to_string(stderr.path())?;
    if !status.success() || stdout.trim() != expected_version {
        return Err(OvmError::Message(format!(
            "updated OVM control plane failed its activation probe: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

fn control_probe_timeout() -> Duration {
    std::env::var("OVM_SELF_UPDATE_PROBE_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or(CONTROL_PROBE_TIMEOUT)
}

pub(super) fn release_version(channel: SelfUpdateChannel) -> Result<String> {
    let override_name = match channel {
        SelfUpdateChannel::Stable => "OVM_SELF_UPDATE_STABLE_VERSION",
        SelfUpdateChannel::Beta => "OVM_SELF_UPDATE_BETA_VERSION",
        SelfUpdateChannel::Alpha => "OVM_SELF_UPDATE_ALPHA_VERSION",
    };
    if let Ok(version) = std::env::var(override_name) {
        return Version::parse(&version)
            .map(|version| version.to_string())
            .map_err(|error| {
                OvmError::Message(format!("Invalid version in {override_name}: {error}"))
            });
    }
    let release = resolve_github_release(channel, &github_api_base(), github_token().as_deref())?;
    parse_tag(&release.tag_name)
        .map(|version| version.to_string())
        .ok_or_else(|| OvmError::Message(format!("Invalid OVM release tag `{}`", release.tag_name)))
}

fn resolve_release(
    channel: SelfUpdateChannel,
    api_base: &str,
    token: Option<&str>,
) -> Result<ResolvedRelease> {
    let release = release_assets(
        resolve_github_release(channel, api_base, token)?,
        api_base,
        token,
    )?;
    let hosts = download_hosts(token);
    // Loopback asset URLs are only legitimate when the metadata api base is
    // itself loopback (the OVM_GITHUB_API_URL test override); production metadata
    // must never resolve to loopback (SSRF guard).
    validate_download_url(&release.archive_url, &hosts, release.allow_loopback)?;
    validate_download_url(&release.checksum_url, &hosts, release.allow_loopback)?;
    Ok(release)
}

fn resolve_github_release(
    channel: SelfUpdateChannel,
    api_base: &str,
    token: Option<&str>,
) -> Result<GithubRelease> {
    match channel {
        SelfUpdateChannel::Stable => {
            let url = format!("{api_base}/repos/{RELEASE_REPOSITORY}/releases/latest");
            let release: GithubRelease = fetch_json(&url, token)?;
            if release.draft || release.prerelease {
                return Err(OvmError::Message(
                    "GitHub's latest OVM release is not a stable release".into(),
                ));
            }
            Ok(release)
        }
        SelfUpdateChannel::Beta => {
            let url = format!("{api_base}/repos/{RELEASE_REPOSITORY}/releases?per_page=100&page=1");
            let releases: Vec<GithubRelease> = fetch_json(&url, token)?;
            select_beta_release(releases)
        }
        SelfUpdateChannel::Alpha => {
            let url = format!("{api_base}/repos/{RELEASE_REPOSITORY}/releases?per_page=15&page=1");
            let releases: Vec<GithubRelease> = fetch_json(&url, token)?;
            select_alpha_release(releases)
        }
    }
}

fn github_api_base() -> String {
    std::env::var("OVM_GITHUB_API_URL").unwrap_or_else(|_| DEFAULT_GITHUB_API.to_string())
}

/// Guard the token-bearing metadata request against an unvalidated
/// `OVM_GITHUB_API_URL`.
///
/// The asset *download* is host-pinned by `validate_download_url`, but the
/// metadata `fetch_json` would otherwise send the `Authorization: Bearer` header
/// to whatever host `OVM_GITHUB_API_URL` names. When a token is present the API
/// base must therefore be api.github.com (the only host OVM authenticates to) or
/// a loopback test mock; any other host is refused loudly so the misconfiguration
/// is visible rather than silently leaking the token.
fn ensure_api_host_allows_token(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| OvmError::Message(format!("invalid GitHub API URL `{url}`: {error}")))?;
    let host = parsed
        .host_str()
        .map(|host| host.trim_end_matches('.').to_ascii_lowercase());
    let is_loopback = matches!(
        host.as_deref(),
        Some("localhost") | Some("127.0.0.1") | Some("[::1]")
    );
    let allowed = matches!(host.as_deref(), Some(GITHUB_ASSET_API_HOST)) || is_loopback;
    if !allowed {
        return Err(OvmError::Message(format!(
            "refusing to send an authenticated GitHub request to `{}`: a token is only ever \
             sent to {GITHUB_ASSET_API_HOST}. Unset OVM_GITHUB_API_URL or point it at \
             https://{GITHUB_ASSET_API_HOST}.",
            host.as_deref().unwrap_or("<no host>")
        )));
    }
    // Host alone is not enough: a token must never ride a plaintext request. Only
    // the loopback test mock may use http; api.github.com must be https so the
    // bearer token can't be sent in the clear (e.g. `http://api.github.com`).
    if !is_loopback && parsed.scheme() != "https" {
        return Err(OvmError::Message(format!(
            "refusing to send an authenticated GitHub request over `{}` to `{}`: a token is only \
             ever sent over HTTPS to {GITHUB_ASSET_API_HOST}.",
            parsed.scheme(),
            host.as_deref().unwrap_or("<no host>")
        )));
    }
    Ok(())
}

/// Hosts the asset download client may fetch from.
///
/// Without a token the set is exactly the stable [`GITHUB_DOWNLOAD_HOSTS`]. With
/// a token the download starts at the api.github.com asset endpoint, so that
/// host is added as the *entry* point; the request is then redirected to a
/// signed `objects.githubusercontent.com` URL, which stable already trusts —
/// the trusted CDN redirect targets are unchanged.
fn download_hosts(token: Option<&str>) -> Vec<&'static str> {
    let mut hosts = GITHUB_DOWNLOAD_HOSTS.to_vec();
    if token.is_some() {
        hosts.push(GITHUB_ASSET_API_HOST);
    }
    hosts
}

fn fetch_json<T: serde::de::DeserializeOwned>(url: &str, token: Option<&str>) -> Result<T> {
    let mut request = http_client(20)?
        .get(url)
        .header("Accept", "application/json");
    if let Some(token) = token {
        // Never attach the bearer to an unvalidated OVM_GITHUB_API_URL host.
        ensure_api_host_allows_token(url)?;
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = request.send()?;
    if !response.status().is_success() {
        return Err(OvmError::DownloadFailed {
            url: url.to_string(),
            message: format!("HTTP {}", response.status()),
        });
    }
    Ok(response.json()?)
}

fn select_beta_release(releases: Vec<GithubRelease>) -> Result<GithubRelease> {
    let versions = releases
        .iter()
        .filter(|release| !release.draft)
        .filter_map(|release| parse_tag(&release.tag_name))
        .collect::<Vec<_>>();
    let selected = latest_beta_newer_than_stable(versions).ok_or_else(|| {
        OvmError::Message("No OVM beta newer than the latest stable release was found".into())
    })?;
    releases
        .into_iter()
        .find(|release| {
            !release.draft
                && release.prerelease
                && parse_tag(&release.tag_name).as_ref() == Some(&selected)
        })
        .ok_or_else(|| OvmError::Message("Selected OVM beta release metadata disappeared".into()))
}

/// Alpha selects the highest-semver release on the repository *including*
/// prereleases (e.g. `v0.2.0-alpha.3`). Unlike beta it does not require the
/// pick to be a prerelease newer than stable: when the newest overall release
/// is the latest stable, alpha simply installs that. `semver::Version`'s
/// ordering is prerelease-aware, so `0.2.0-alpha.1 < 0.2.0-alpha.2 < 0.2.0`.
fn select_alpha_release(releases: Vec<GithubRelease>) -> Result<GithubRelease> {
    let selected = releases
        .iter()
        .filter(|release| !release.draft)
        .filter_map(|release| parse_tag(&release.tag_name))
        .max()
        .ok_or_else(|| OvmError::Message("No OVM release was found on the alpha channel".into()))?;
    releases
        .into_iter()
        .find(|release| !release.draft && parse_tag(&release.tag_name).as_ref() == Some(&selected))
        .ok_or_else(|| OvmError::Message("Selected OVM alpha release metadata disappeared".into()))
}

fn parse_tag(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

fn release_assets(
    release: GithubRelease,
    api_base: &str,
    token: Option<&str>,
) -> Result<ResolvedRelease> {
    let version = parse_tag(&release.tag_name)
        .ok_or_else(|| {
            OvmError::Message(format!("Invalid OVM release tag `{}`", release.tag_name))
        })?
        .to_string();
    let target = target_triple(std::env::consts::OS, std::env::consts::ARCH)?;
    let archive_name = format!("ovm-{target}.tar.gz");
    let checksum_name = format!("{archive_name}.sha256");
    let archive_url = asset_download_url(&release.assets, &archive_name, api_base, token)?;
    let checksum_url = asset_download_url(&release.assets, &checksum_name, api_base, token)?;
    require_release_repository(&archive_url)?;
    require_release_repository(&checksum_url)?;

    Ok(ResolvedRelease {
        version,
        archive_name,
        archive_url,
        checksum_url,
        allow_loopback: api_base_is_loopback(api_base),
    })
}

/// Whether the metadata API base resolves to a loopback host. This is only ever
/// the case under the `OVM_GITHUB_API_URL` test override pointing at a local
/// mock; in production it is `api.github.com`. Used to gate whether a
/// metadata-supplied loopback asset URL may be fetched (see [`ResolvedRelease`]).
fn api_base_is_loopback(api_base: &str) -> bool {
    reqwest::Url::parse(api_base)
        .ok()
        .and_then(|url| {
            url.host_str()
                .map(|host| host.trim_end_matches('.').to_string())
        })
        .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]"))
}

fn require_release_repository(url: &str) -> Result<()> {
    // Defense in depth: GitHub-hosted assets must come from this project's
    // own releases, not merely any URL an API response names. Parse the URL
    // and check the host and path *components* — a substring match on the
    // whole URL is bypassable with a query/fragment like
    // `?x=/ovm-sh/ovm/releases/`.
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| OvmError::Message(format!("Release asset URL is invalid: {error}")))?;
    // Normalize a trailing FQDN dot: `github.com.` resolves to the same host,
    // and the download client strips it, so it must not skip this check.
    let host = parsed
        .host_str()
        .map(|host| host.trim_end_matches('.').to_ascii_lowercase());
    match host.as_deref() {
        // Public path: browser_download_url on github.com.
        Some("github.com") => {
            let prefix = format!("/{RELEASE_REPOSITORY}/releases/");
            if !parsed.path().starts_with(&prefix) {
                return Err(OvmError::Message(format!(
                    "Release asset URL is outside {RELEASE_REPOSITORY}: {url}"
                )));
            }
        }
        // Authenticated path: the API asset endpoint for the SAME pinned repo,
        // `/repos/<slug>/releases/assets/<id>`. Nothing else on api.github.com
        // is accepted.
        Some(GITHUB_ASSET_API_HOST) => {
            let prefix = format!("/repos/{RELEASE_REPOSITORY}/releases/assets/");
            if !parsed.path().starts_with(&prefix) {
                return Err(OvmError::Message(format!(
                    "Release asset API URL is outside {RELEASE_REPOSITORY}: {url}"
                )));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Choose the download URL for a named release asset.
///
/// Without a token we keep the public `browser_download_url` byte-for-byte (it
/// works unauthenticated on public repos). With a token we must instead use the
/// API asset endpoint: `browser_download_url` returns 404 for a private-repo
/// asset even with a bearer token, whereas
/// `/repos/<slug>/releases/assets/<id>` (fetched with
/// `Accept: application/octet-stream`) redirects to a signed CDN URL that
/// delivers the bytes.
fn asset_download_url(
    assets: &[GithubAsset],
    name: &str,
    api_base: &str,
    token: Option<&str>,
) -> Result<String> {
    let asset = assets
        .iter()
        .find(|asset| asset.name == name)
        .ok_or_else(|| OvmError::Message(format!("OVM release is missing asset `{name}`")))?;
    match token {
        Some(_) => Ok(format!(
            "{api_base}/repos/{RELEASE_REPOSITORY}/releases/assets/{}",
            asset.id
        )),
        None => Ok(asset.browser_download_url.clone()),
    }
}

fn target_triple(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        _ => Err(OvmError::Message(format!(
            "OVM direct updates do not support {os}-{arch}"
        ))),
    }
}

fn download(
    url: &str,
    destination: &Path,
    token: Option<&str>,
    allow_loopback: bool,
) -> Result<()> {
    let hosts = download_hosts(token);
    validate_download_url(url, &hosts, allow_loopback)?;
    let mut request = download_http_client(120, &hosts)?.get(url);
    if let Some(token) = token {
        // The API asset endpoint needs an explicit octet-stream Accept (it
        // returns asset JSON otherwise) plus the bearer token. reqwest strips
        // Authorization on cross-host/cross-port redirects
        // (reqwest::redirect::remove_sensitive_headers), so the token is never
        // forwarded to the signed objects.githubusercontent.com CDN it bounces
        // to; that URL authenticates itself via its own query string.
        request = request
            .header("Accept", "application/octet-stream")
            .header("Authorization", format!("Bearer {token}"));
    }
    // `url` is always the api.github.com/github.com endpoint we constructed —
    // never the signed redirect target — so it is safe to echo in errors. A
    // transport error from `send()`, however, may reference the signed URL
    // whose query string carries a download credential; on the authenticated
    // path strip the URL from that error so the credential can never leak.
    let mut response = request.send().map_err(|error| {
        if token.is_some() {
            OvmError::from(error.without_url())
        } else {
            OvmError::from(error)
        }
    })?;
    if !response.status().is_success() {
        return Err(OvmError::DownloadFailed {
            url: url.to_string(),
            message: format!("HTTP {}", response.status()),
        });
    }
    let mut output = File::create(destination)?;
    std::io::copy(&mut response, &mut output)?;
    output.flush()?;
    output.sync_all()?;
    Ok(())
}

fn verify_checksum(archive: &Path, checksum: &Path, archive_name: &str) -> Result<()> {
    let contents = std::fs::read_to_string(checksum)?;
    let mut fields = contents.split_whitespace();
    let expected = fields
        .next()
        .ok_or_else(|| OvmError::Message("OVM checksum file is empty".into()))?;
    let named = fields
        .next()
        .ok_or_else(|| OvmError::Message("OVM checksum file has no archive name".into()))?
        .trim_start_matches('*');
    if fields.next().is_some()
        || expected.len() != 64
        || !expected.bytes().all(|byte| byte.is_ascii_hexdigit())
        || named != archive_name
    {
        return Err(OvmError::Message(
            "OVM checksum file has an invalid format".into(),
        ));
    }

    let mut file = File::open(archive)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hex_digest(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(OvmError::Message(
            "OVM release archive checksum mismatch".into(),
        ));
    }
    Ok(())
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn extract_bundle(archive_path: &Path, destination: &Path) -> Result<BundleManifest> {
    std::fs::create_dir_all(destination)?;
    let decoder = GzDecoder::new(File::open(archive_path)?);
    let mut archive = Archive::new(decoder);
    let mut names = HashSet::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            return Err(OvmError::ExtractionFailed(
                "OVM bundle contains a non-regular entry".into(),
            ));
        }
        let path = entry.path()?.into_owned();
        // Effective (PAX-aware) size, not the raw header size: a PAX `size`
        // extended header overrides the header and drives how many bytes
        // `unpack` streams, so validating the header alone could be bypassed.
        crate::sources::validate_tar_entry_size(entry.size(), &path)?;
        let name = top_level_name(&path)?;
        if name != BUNDLE_MANIFEST_NAME && !safe_archive_binary(&name) {
            return Err(OvmError::ExtractionFailed(format!(
                "OVM bundle contains unexpected entry `{name}`"
            )));
        }
        if !names.insert(name.clone()) {
            return Err(OvmError::ExtractionFailed(format!(
                "OVM bundle repeats entry `{name}`"
            )));
        }
        entry.unpack(destination.join(name))?;
    }

    let manifest = BundleManifest::load(&destination.join(BUNDLE_MANIFEST_NAME))?;
    let expected = std::iter::once(BUNDLE_MANIFEST_NAME.to_string())
        .chain(manifest.binary_names().map(str::to_string))
        .collect::<HashSet<_>>();
    if names != expected {
        return Err(OvmError::ExtractionFailed(
            "OVM bundle contents do not match its manifest".into(),
        ));
    }
    Ok(manifest)
}

fn top_level_name(path: &Path) -> Result<String> {
    let mut components = path.components();
    let name = match components.next() {
        Some(Component::Normal(name)) => name
            .to_str()
            .ok_or_else(|| OvmError::ExtractionFailed("non-UTF-8 bundle entry".into()))?,
        _ => {
            return Err(OvmError::ExtractionFailed(
                "OVM bundle contains an unsafe path".into(),
            ))
        }
    };
    if components.next().is_some() {
        return Err(OvmError::ExtractionFailed(
            "OVM bundle entries must be top-level files".into(),
        ));
    }
    Ok(name.to_string())
}

fn safe_archive_binary(name: &str) -> bool {
    name == "ovm"
        || name.strip_prefix("ovm-").is_some_and(|suffix| {
            !suffix.is_empty()
                && !suffix.starts_with('-')
                && !suffix.ends_with('-')
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && !suffix.as_bytes().windows(2).any(|pair| pair == b"--")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle_manifest::BundleManifest;
    use crate::config::OvmDirs;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use mockito::Server;
    use tar::Builder;
    use tempfile::tempdir;

    fn release(tag: &str, draft: bool, prerelease: bool) -> GithubRelease {
        let target = target_triple(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        GithubRelease {
            tag_name: tag.into(),
            draft,
            prerelease,
            assets: vec![
                GithubAsset {
                    id: 1,
                    name: format!("ovm-{target}.tar.gz"),
                    browser_download_url: "https://github.com/archive".into(),
                },
                GithubAsset {
                    id: 2,
                    name: format!("ovm-{target}.tar.gz.sha256"),
                    browser_download_url: "https://github.com/checksum".into(),
                },
            ],
        }
    }

    fn write_archive(path: &Path, manifest: &str, binaries: &[&str], extra: Option<&str>) {
        let file = File::create(path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);
        append(&mut builder, BUNDLE_MANIFEST_NAME, manifest.as_bytes());
        for binary in binaries {
            append(&mut builder, binary, binary.as_bytes());
        }
        if let Some(extra) = extra {
            append(&mut builder, extra, b"extra");
        }
        builder.finish().unwrap();
    }

    fn append(builder: &mut Builder<GzEncoder<File>>, name: &str, contents: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, name, contents).unwrap();
    }

    #[test]
    fn maps_supported_targets() {
        assert_eq!(
            target_triple("macos", "aarch64").unwrap(),
            "aarch64-apple-darwin"
        );
        assert_eq!(
            target_triple("linux", "x86_64").unwrap(),
            "x86_64-unknown-linux-gnu"
        );
        assert!(target_triple("windows", "x86_64").is_err());
    }

    #[test]
    fn selects_beta_newer_than_stable() {
        let selected = select_beta_release(vec![
            release("v0.1.0", false, false),
            release("v0.2.0-beta.1", false, true),
            release("v0.2.0-beta.2", false, true),
            release("v0.3.0-beta.1", true, true),
        ])
        .unwrap();
        assert_eq!(selected.tag_name, "v0.2.0-beta.2");
    }

    #[test]
    fn alpha_selects_highest_semver_including_prereleases() {
        // Highest overall is the prerelease; a draft is ignored.
        let selected = select_alpha_release(vec![
            release("v0.1.0", false, false),
            release("v0.2.0-alpha.1", false, true),
            release("v0.2.0-alpha.3", false, true),
            release("v0.3.0-alpha.1", true, true),
        ])
        .unwrap();
        assert_eq!(selected.tag_name, "v0.2.0-alpha.3");
    }

    #[test]
    fn alpha_installs_stable_when_it_is_the_newest_overall() {
        // A released 0.2.0 outranks its own prereleases, so alpha lands it.
        let selected = select_alpha_release(vec![
            release("v0.2.0-alpha.1", false, true),
            release("v0.2.0-alpha.2", false, true),
            release("v0.2.0", false, false),
        ])
        .unwrap();
        assert_eq!(selected.tag_name, "v0.2.0");
    }

    #[test]
    fn stable_channel_rejects_a_prerelease_latest() {
        // releases/latest is normally prerelease-free, but a mislabeled or
        // hand-crafted response served there must not sneak an alpha onto the
        // stable channel.
        let mut server = Server::new();
        let metadata = serde_json::json!({
            "tag_name": "v0.2.0-alpha.1",
            "draft": false,
            "prerelease": true,
            "assets": []
        });
        server
            .mock("GET", "/repos/ovm-sh/ovm-oss/releases/latest")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(metadata.to_string())
            .create();

        let error = resolve_github_release(SelfUpdateChannel::Stable, &server.url(), None)
            .expect_err("a prerelease latest must be rejected on stable");
        assert!(
            error.to_string().contains("not a stable release"),
            "{error}"
        );
    }

    #[test]
    fn alpha_channel_selects_prerelease_from_mock_list() {
        let mut server = Server::new();
        let metadata = serde_json::json!([
            {"tag_name": "v0.1.0", "draft": false, "prerelease": false, "assets": []},
            {"tag_name": "v0.2.0-alpha.2", "draft": false, "prerelease": true, "assets": []},
            {"tag_name": "v0.2.0-alpha.1", "draft": false, "prerelease": true, "assets": []}
        ]);
        server
            .mock("GET", "/repos/ovm-sh/ovm-oss/releases")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(metadata.to_string())
            .create();

        let release =
            resolve_github_release(SelfUpdateChannel::Alpha, &server.url(), None).unwrap();
        assert_eq!(release.tag_name, "v0.2.0-alpha.2");
    }

    #[test]
    fn update_decision_is_prerelease_aware() {
        let v = |s: &str| Version::parse(s).unwrap();
        // Alpha upgrade proceeds.
        assert_eq!(
            update_decision(&v("0.1.0"), &v("0.2.0-alpha.1")),
            UpdateDecision::Proceed
        );
        // Newer alpha over older alpha proceeds.
        assert_eq!(
            update_decision(&v("0.2.0-alpha.1"), &v("0.2.0-alpha.3")),
            UpdateDecision::Proceed
        );
        // Same version is a no-op.
        assert_eq!(
            update_decision(&v("0.2.0-alpha.3"), &v("0.2.0-alpha.3")),
            UpdateDecision::AlreadyLatest
        );
        // Alpha is newer than the release-line stable it precedes -> refuse.
        assert_eq!(
            update_decision(&v("0.2.0-alpha.3"), &v("0.1.0")),
            UpdateDecision::Downgrade
        );
        // Alpha precedes its own final release -> refuse a stable "downgrade".
        assert_eq!(
            update_decision(&v("0.2.0"), &v("0.2.0-alpha.1")),
            UpdateDecision::Downgrade
        );
    }

    #[test]
    fn self_update_gate_uses_selected_version_not_compiled() {
        let compiled = "0.2.0";

        // The regression: on a dev snapshot, an explicit update to the release
        // that equals the compiled version must PROCEED (no spurious
        // AlreadyLatest) — dev is unordered against releases.
        assert!(selection_gate(Some("dev-abc123"), compiled, "0.2.0").is_none());

        // A real selected version equal to the target is AlreadyLatest,
        // independent of the compiled constant.
        let (current, target) = selection_gate(Some("0.1.0"), compiled, "0.1.0").unwrap();
        assert_eq!(
            update_decision(&current, &target),
            UpdateDecision::AlreadyLatest
        );

        // Selected newer than the target refuses the downgrade.
        let (current, target) = selection_gate(Some("0.3.0"), compiled, "0.1.0").unwrap();
        assert_eq!(
            update_decision(&current, &target),
            UpdateDecision::Downgrade
        );

        // Selected older than the target proceeds.
        let (current, target) = selection_gate(Some("0.1.0"), compiled, "0.2.0").unwrap();
        assert_eq!(update_decision(&current, &target), UpdateDecision::Proceed);

        // No selection yet falls back to the compiled version as the baseline.
        let (current, target) = selection_gate(None, compiled, "0.2.0").unwrap();
        assert_eq!(
            update_decision(&current, &target),
            UpdateDecision::AlreadyLatest
        );

        // Unparseable target simply proceeds (the gate never blocks on junk).
        assert!(selection_gate(Some("0.1.0"), compiled, "not-semver").is_none());
    }

    #[test]
    fn downgrade_message_flags_alpha_ahead_of_stable() {
        let v = |s: &str| Version::parse(s).unwrap();
        let from_alpha = downgrade_message(&v("0.2.0-alpha.3"), &v("0.1.0"));
        assert!(from_alpha.contains("Refusing to downgrade"), "{from_alpha}");
        assert!(
            from_alpha.contains("alpha channel is ahead"),
            "{from_alpha}"
        );

        let from_stable = downgrade_message(&v("0.2.0"), &v("0.1.0"));
        assert!(
            !from_stable.contains("alpha channel is ahead"),
            "{from_stable}"
        );
    }

    #[test]
    fn verifies_strict_checksum_format() {
        let temp = tempdir().unwrap();
        let archive = temp.path().join("ovm.tar.gz");
        std::fs::write(&archive, b"bundle").unwrap();
        let digest = hex_digest(Sha256::digest(b"bundle"));
        let checksum = temp.path().join("ovm.tar.gz.sha256");
        std::fs::write(&checksum, format!("{digest}  ovm.tar.gz\n")).unwrap();
        verify_checksum(&archive, &checksum, "ovm.tar.gz").unwrap();

        std::fs::write(&checksum, format!("{digest}  other.tar.gz\n")).unwrap();
        assert!(verify_checksum(&archive, &checksum, "ovm.tar.gz").is_err());
    }

    #[test]
    fn release_repository_check_uses_path_not_substring() {
        // The genuine asset URL passes.
        require_release_repository(
            "https://github.com/ovm-sh/ovm-oss/releases/download/v0.0.1/ovm.tar.gz",
        )
        .unwrap();
        // A foreign github.com repo that smuggles the expected path into the
        // query string must be rejected — the old substring check let it pass.
        assert!(require_release_repository(
            "https://github.com/attacker/evil/releases/download/v9.9.9/ovm.tar.gz?x=/ovm-sh/ovm/releases/",
        )
        .is_err());
        // A non-github host is out of scope for this check (TLS + host pinning
        // in the download client governs it), so it is not rejected here.
        require_release_repository("https://objects.githubusercontent.com/foo/bar").unwrap();
        // A trailing-dot FQDN resolves to github.com and must not skip the
        // repository-path check.
        assert!(require_release_repository(
            "https://github.com./attacker/evil/releases/download/v9.9.9/ovm.tar.gz",
        )
        .is_err());
        // Userinfo must not fool host parsing: host is still github.com, so
        // the repository-path check still applies and a foreign path fails.
        assert!(require_release_repository(
            "https://user@github.com/attacker/evil/releases/download/v9.9.9/ovm.tar.gz",
        )
        .is_err());
        require_release_repository(
            "https://user@github.com/ovm-sh/ovm-oss/releases/download/v0.0.1/ovm.tar.gz",
        )
        .unwrap();
    }

    #[test]
    fn blocks_termination_signals_during_activation() {
        unsafe {
            let mut before = std::mem::zeroed();
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut before),
                0
            );
            let guard = BlockedTerminationSignals::new().unwrap();
            let mut blocked = std::mem::zeroed();
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut blocked),
                0
            );
            assert_eq!(libc::sigismember(&blocked, libc::SIGINT), 1);
            assert_eq!(libc::sigismember(&blocked, libc::SIGTERM), 1);
            drop(guard);
            let mut restored = std::mem::zeroed();
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut restored),
                0
            );
            assert_eq!(
                libc::sigismember(&restored, libc::SIGINT),
                libc::sigismember(&before, libc::SIGINT)
            );
            assert_eq!(
                libc::sigismember(&restored, libc::SIGTERM),
                libc::sigismember(&before, libc::SIGTERM)
            );
        }
    }

    #[test]
    fn times_out_a_hanging_control_plane_probe() {
        let temp = tempdir().unwrap();
        let manager = SelfManager::at(OvmDirs::at(temp.path().join("home/.ovm")));
        manager.ensure_dirs().unwrap();
        let control = manager.control_plane_path();
        std::fs::write(&control, "#!/bin/sh\nexec sleep 5\n").unwrap();
        crate::util::make_executable(&control).unwrap();

        let started = Instant::now();
        let error = probe_control_plane_with_timeout(&manager, "never", Duration::from_millis(100))
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn extracts_dynamic_manifest_bundle() {
        let temp = tempdir().unwrap();
        let archive = temp.path().join("bundle.tar.gz");
        let manifest = "ovm-bundle-v1\nmain\tovm\tovm\nside\tovm-one\tovm-one\nside\tovm-two\t-\n";
        write_archive(&archive, manifest, &["ovm", "ovm-one", "ovm-two"], None);
        let parsed = extract_bundle(&archive, &temp.path().join("out")).unwrap();
        assert_eq!(parsed, BundleManifest::parse(manifest).unwrap());
    }

    #[test]
    fn rejects_manifest_archive_drift() {
        let temp = tempdir().unwrap();
        let manifest = "ovm-bundle-v1\nmain\tovm\tovm\nside\tovm-one\tovm-one\n";

        let missing = temp.path().join("missing.tar.gz");
        write_archive(&missing, manifest, &["ovm"], None);
        assert!(extract_bundle(&missing, &temp.path().join("missing")).is_err());

        let extra = temp.path().join("extra.tar.gz");
        write_archive(&extra, manifest, &["ovm", "ovm-one"], Some("ovm-extra"));
        assert!(extract_bundle(&extra, &temp.path().join("extra")).is_err());
    }

    #[test]
    fn rejects_hostile_archive_entries() {
        let temp = tempdir().unwrap();
        let manifest = "ovm-bundle-v1\nmain\tovm\tovm\n";

        // Symlink entry, path traversal, absolute path, nested path, and a
        // directory entry must each fail extraction, not just be skipped.
        let build = |name: &str, hostile: &dyn Fn(&mut Builder<GzEncoder<File>>)| {
            let path = temp.path().join(format!("{name}.tar.gz"));
            let file = File::create(&path).unwrap();
            let mut builder = Builder::new(GzEncoder::new(file, Compression::default()));
            append(&mut builder, BUNDLE_MANIFEST_NAME, manifest.as_bytes());
            append(&mut builder, "ovm", b"ovm");
            hostile(&mut builder);
            builder.into_inner().unwrap().finish().unwrap();
            path
        };

        let symlink = build("symlink", &|builder| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_cksum();
            builder
                .append_link(&mut header, "ovm-evil", "/etc/passwd")
                .unwrap();
        });
        assert!(extract_bundle(&symlink, &temp.path().join("s")).is_err());

        // tar-rs refuses to author `..` or absolute paths at all — those
        // shapes need a hand-forged header even to exist. `top_level_name`
        // rejects any path that isn't a single Normal component, which the
        // nested case exercises.
        let nested = build("nested", &|builder| {
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "sub/ovm-evil", &b"evil"[..])
                .unwrap();
        });
        assert!(extract_bundle(&nested, &temp.path().join("nested")).is_err());

        let dir_entry = build("dir", &|builder| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_cksum();
            builder
                .append_data(&mut header, "ovm-dir", &b""[..])
                .unwrap();
        });
        assert!(extract_bundle(&dir_entry, &temp.path().join("d")).is_err());
    }

    #[test]
    fn update_failure_guides_manual_recovery_when_rollback_also_fails() {
        // A failed activation whose automatic rollback ALSO fails (e.g. the
        // previous bundle is damaged): the user must be told how to recover by
        // hand rather than left with a bare error or a half-switched control
        // plane.
        let activation = OvmError::Message("activation probe failed".into());
        let recovery = Err(OvmError::Message("previous bundle is corrupt".into()));
        let message = update_failure(activation, recovery).to_string();

        assert!(message.contains("activation probe failed"), "{message}");
        assert!(
            message.contains("automatic recovery also failed"),
            "{message}"
        );
        assert!(message.contains("previous bundle is corrupt"), "{message}");
        // Actionable manual-recovery guidance.
        assert!(message.contains("ovm self repair"), "{message}");
        assert!(message.contains("reinstall OVM"), "{message}");
    }

    #[test]
    fn update_failure_without_recovery_error_is_unchanged() {
        let activation = OvmError::Message("activation probe failed".into());
        let message = update_failure(activation, Ok(())).to_string();
        assert_eq!(message, "activation probe failed");
    }

    #[test]
    fn rejects_bundle_entry_with_oversized_declared_size() {
        let temp = tempdir().unwrap();
        let archive = temp.path().join("bomb.tar.gz");
        // A single `ovm` entry whose header claims ~8 GiB (above the 4 GiB cap)
        // with no data behind it. Extraction must reject on the declared size
        // before reading or writing the entry.
        write_declared_size_archive(&archive, "ovm", 0o77777777777);

        let error = extract_bundle(&archive, &temp.path().join("out"))
            .expect_err("oversized bundle entry must be rejected");
        assert!(error.to_string().contains("oversized"), "{error}");
    }

    /// Write a tar.gz whose single entry's header *declares* `declared_size`
    /// bytes while carrying no data.
    fn write_declared_size_archive(path: &Path, entry_name: &str, declared_size: u64) {
        let mut header = [0u8; 512];
        let name = entry_name.as_bytes();
        let len = name.len().min(99);
        header[..len].copy_from_slice(&name[..len]);
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size_str = format!("{declared_size:011o}\0");
        header[124..136].copy_from_slice(size_str.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[156] = b'0'; // regular file
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].copy_from_slice(b"        ");
        let cksum: u32 = header.iter().map(|&b| b as u32).sum();
        let cksum_str = format!("{cksum:06o}\0 ");
        header[148..156].copy_from_slice(&cksum_str.as_bytes()[..8]);

        let end = [0u8; 1024];
        let file = File::create(path).unwrap();
        let mut gz = GzEncoder::new(file, Compression::default());
        gz.write_all(&header).unwrap();
        gz.write_all(&end).unwrap();
        gz.finish().unwrap();
    }

    #[test]
    fn prepares_verified_bundle_from_mock_release() {
        let temp = tempdir().unwrap();
        let archive_path = temp.path().join("release.tar.gz");
        let manifest = "ovm-bundle-v1\nmain\tovm\tovm\nside\tovm-side\tovm-side\n";
        write_archive(&archive_path, manifest, &["ovm", "ovm-side"], None);
        let archive_bytes = std::fs::read(&archive_path).unwrap();
        let digest = hex_digest(Sha256::digest(&archive_bytes));
        let target = target_triple(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let archive_name = format!("ovm-{target}.tar.gz");

        let mut server = Server::new();
        let archive_url = format!("{}/archive", server.url());
        let checksum_url = format!("{}/checksum", server.url());
        let metadata = serde_json::json!({
            "tag_name": "v0.1.0",
            "draft": false,
            "prerelease": false,
            "assets": [
                {"name": archive_name, "browser_download_url": archive_url},
                {"name": format!("{archive_name}.sha256"), "browser_download_url": checksum_url}
            ]
        });
        let metadata_mock = server
            .mock("GET", "/repos/ovm-sh/ovm-oss/releases/latest")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(metadata.to_string())
            .create();
        let archive_mock = server
            .mock("GET", "/archive")
            .with_status(200)
            .with_body(archive_bytes)
            .create();
        let checksum_mock = server
            .mock("GET", "/checksum")
            .with_status(200)
            .with_body(format!("{digest}  {archive_name}\n"))
            .create();

        let bundle = prepare_from_api(SelfUpdateChannel::Stable, &server.url(), None).unwrap();
        assert_eq!(bundle.release.version, "0.1.0");
        assert_eq!(bundle.manifest, BundleManifest::parse(manifest).unwrap());
        assert!(bundle.source_dir.join("ovm-side").is_file());
        metadata_mock.assert();
        archive_mock.assert();
        checksum_mock.assert();
    }

    // A runnable `ovm` for the activation probe: it resolves `../self/current`
    // relative to its own install location (the probe spawns it with no special
    // env) and prints the active version, mirroring the real control plane.
    fn ovm_probe_ok() -> &'static str {
        "#!/bin/sh\nif [ \"$1\" = self ] && [ \"$2\" = current ]; then\n  dir=$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\n  basename \"$(readlink \"$dir/../self/current\")\"\n  exit 0\nfi\nexit 0\n"
    }

    // A broken replacement control plane: fails its probe like a bad release.
    fn ovm_probe_broken() -> &'static str {
        "#!/bin/sh\nexit 1\n"
    }

    fn ovm_archive(path: &Path, ovm_script: &str) -> Vec<u8> {
        let file = File::create(path).unwrap();
        let mut builder = Builder::new(GzEncoder::new(file, Compression::default()));
        append(
            &mut builder,
            BUNDLE_MANIFEST_NAME,
            b"ovm-bundle-v1\nmain\tovm\tovm\n",
        );
        append(&mut builder, "ovm", ovm_script.as_bytes());
        // Finish the tar *and* the gzip stream before reading the file back, or
        // the gzip trailer is still buffered and the archive reads as truncated.
        builder.into_inner().unwrap().finish().unwrap();
        std::fs::read(path).unwrap()
    }

    fn serve_latest(server: &mut Server, tag: &str, archive: &[u8]) {
        let target = target_triple(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let archive_name = format!("ovm-{target}.tar.gz");
        let digest = hex_digest(Sha256::digest(archive));
        let metadata = serde_json::json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "assets": [
                {"name": archive_name, "browser_download_url": format!("{}/archive", server.url())},
                {"name": format!("{archive_name}.sha256"), "browser_download_url": format!("{}/checksum", server.url())}
            ]
        });
        server
            .mock("GET", "/repos/ovm-sh/ovm-oss/releases/latest")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(metadata.to_string())
            .create();
        server
            .mock("GET", "/archive")
            .with_status(200)
            .with_body(archive)
            .create();
        server
            .mock("GET", "/checksum")
            .with_status(200)
            .with_body(format!("{digest}  {archive_name}\n"))
            .create();
    }

    fn install_active(manager: &SelfManager, version: &str, workspace: &Path) {
        let source = workspace.join(format!("src-{version}"));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join(BUNDLE_MANIFEST_NAME),
            "ovm-bundle-v1\nmain\tovm\tovm\n",
        )
        .unwrap();
        let ovm = source.join("ovm");
        std::fs::write(&ovm, ovm_probe_ok()).unwrap();
        crate::util::make_executable(&ovm).unwrap();
        let manifest = BundleManifest::parse("ovm-bundle-v1\nmain\tovm\tovm\n").unwrap();
        manager.install_bundle(version, &manifest, &source).unwrap();
        manager.refresh_control_plane(version).unwrap();
        manager.use_version(version).unwrap();
    }

    #[test]
    fn self_update_installs_and_activates_a_release() {
        let temp = tempdir().unwrap();
        let manager = SelfManager::at(OvmDirs::at(temp.path().join(".ovm")));
        let archive = ovm_archive(&temp.path().join("release.tar.gz"), ovm_probe_ok());
        let mut server = Server::new();
        serve_latest(&mut server, "v0.9.9", &archive);

        update_with(
            &manager,
            SelfUpdateChannel::Stable,
            &server.url(),
            None,
            false,
        )
        .unwrap();

        assert_eq!(manager.current_version().unwrap().as_deref(), Some("0.9.9"));
        assert!(manager.require_complete("0.9.9").is_ok());
        assert!(manager.control_plane_path().is_file());
    }

    #[test]
    fn self_update_rolls_back_when_activation_probe_fails() {
        let temp = tempdir().unwrap();
        let manager = SelfManager::at(OvmDirs::at(temp.path().join(".ovm")));
        install_active(&manager, "0.1.0", temp.path());
        let good_control = std::fs::read(manager.control_plane_path()).unwrap();

        let archive = ovm_archive(&temp.path().join("bad.tar.gz"), ovm_probe_broken());
        let mut server = Server::new();
        serve_latest(&mut server, "v0.9.9", &archive);

        let error = update_with(
            &manager,
            SelfUpdateChannel::Stable,
            &server.url(),
            None,
            false,
        )
        .expect_err("a failed activation probe must roll back");
        assert!(error.to_string().contains("probe"), "{error}");

        // The prior version stays active and its control plane is restored.
        assert_eq!(manager.current_version().unwrap().as_deref(), Some("0.1.0"));
        assert_eq!(
            std::fs::read(manager.control_plane_path()).unwrap(),
            good_control
        );
    }

    // Serializes the tests that mutate the token environment variables so they
    // do not observe one another's values under the parallel test runner.
    static TOKEN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn github_token_uses_only_ovm_var_never_ambient_github_token() {
        let _guard = TOKEN_ENV_LOCK.lock().expect("token env lock");
        std::env::remove_var("OVM_GITHUB_TOKEN");
        std::env::remove_var("GITHUB_TOKEN");
        assert_eq!(github_token(), None);

        // An ambient GITHUB_TOKEN (inherited from the shell or CI) must NEVER be
        // adopted — only the OVM-specific var is honored.
        std::env::set_var("GITHUB_TOKEN", "ambient");
        assert_eq!(github_token(), None);

        // OVM_GITHUB_TOKEN is the sole source of the token.
        std::env::set_var("OVM_GITHUB_TOKEN", "primary");
        assert_eq!(github_token().as_deref(), Some("primary"));

        // A blank OVM_GITHUB_TOKEN is treated as absent — and still no fallback
        // to the ambient GITHUB_TOKEN.
        std::env::set_var("OVM_GITHUB_TOKEN", "   ");
        assert_eq!(github_token(), None);

        std::env::remove_var("OVM_GITHUB_TOKEN");
        std::env::remove_var("GITHUB_TOKEN");
    }

    #[test]
    fn authenticated_request_refuses_non_github_api_host() {
        // api.github.com is the only authenticated destination.
        ensure_api_host_allows_token("https://api.github.com/repos/ovm-sh/ovm-oss/releases/latest")
            .expect("api.github.com must be allowed");
        // Loopback test mocks stay usable so the mock-server tests keep working.
        ensure_api_host_allows_token("http://127.0.0.1:1234/repos/ovm-sh/ovm-oss/releases/latest")
            .expect("loopback mock must be allowed");
        ensure_api_host_allows_token("http://localhost:1234/repos/ovm-sh/ovm-oss/releases/latest")
            .expect("localhost mock must be allowed");

        // An attacker-controlled OVM_GITHUB_API_URL host must be refused before
        // any bearer token can leave — a clear error, not a silent send.
        let error = ensure_api_host_allows_token(
            "https://evil.example.com/repos/ovm-sh/ovm-oss/releases/latest",
        )
        .expect_err("a non-api.github.com host must be refused when a token is present");
        assert!(error.to_string().contains("evil.example.com"), "{error}");
        assert!(
            error.to_string().contains("api.github.com"),
            "the refusal must name the only authenticated host: {error}"
        );

        // A trailing-dot FQDN resolves to the same host and must not bypass it.
        assert!(ensure_api_host_allows_token(
            "https://attacker.example.com./repos/ovm-sh/ovm-oss/releases/latest"
        )
        .is_err());

        // Right host, wrong scheme: a token must never ride plaintext HTTP to a
        // public host. `http://api.github.com` must be refused.
        let error = ensure_api_host_allows_token(
            "http://api.github.com/repos/ovm-sh/ovm-oss/releases/latest",
        )
        .expect_err("plaintext http to api.github.com must be refused when a token is present");
        assert!(error.to_string().contains("HTTPS"), "{error}");
        // Any non-loopback plaintext host is likewise refused.
        assert!(ensure_api_host_allows_token(
            "http://evil.example.com/repos/ovm-sh/ovm-oss/releases/latest"
        )
        .is_err());
    }

    #[test]
    fn loopback_asset_url_allowed_only_under_test_api_base() {
        // Production: api.github.com metadata must never authorize a loopback
        // asset URL (SSRF guard) — allow_loopback stays false.
        assert!(!api_base_is_loopback("https://api.github.com"));
        assert!(!api_base_is_loopback("https://api.github.com."));
        // Test override: a loopback api base legitimately serves loopback assets.
        assert!(api_base_is_loopback("http://127.0.0.1:1234"));
        assert!(api_base_is_loopback("http://localhost:1234"));
    }

    #[test]
    fn token_metadata_fetch_refuses_attacker_api_base() {
        // End-to-end: with a token set, resolving a release against an attacker
        // API base must error out (no bearer leaves) rather than authenticate to
        // the attacker host. No mock is registered, proving no request is sent.
        let error = resolve_github_release(
            SelfUpdateChannel::Stable,
            "https://evil.example.com",
            Some("secret-token"),
        )
        .expect_err("a token must never be sent to an attacker-controlled API base");
        assert!(error.to_string().contains("evil.example.com"), "{error}");
    }

    #[test]
    fn asset_download_url_switches_on_token() {
        let assets = vec![GithubAsset {
            id: 42,
            name: "ovm.tar.gz".into(),
            browser_download_url:
                "https://github.com/ovm-sh/ovm-oss/releases/download/v1/ovm.tar.gz".into(),
        }];
        // No token: the public browser_download_url is kept verbatim.
        assert_eq!(
            asset_download_url(&assets, "ovm.tar.gz", "https://api.github.com", None).unwrap(),
            "https://github.com/ovm-sh/ovm-oss/releases/download/v1/ovm.tar.gz"
        );
        // Token: the API asset endpoint keyed by the asset id.
        assert_eq!(
            asset_download_url(&assets, "ovm.tar.gz", "https://api.github.com", Some("tok"))
                .unwrap(),
            "https://api.github.com/repos/ovm-sh/ovm-oss/releases/assets/42"
        );
    }

    #[test]
    fn asset_api_url_pinned_to_release_repository() {
        // The API asset endpoint for the pinned repo passes.
        require_release_repository(
            "https://api.github.com/repos/ovm-sh/ovm-oss/releases/assets/42",
        )
        .unwrap();
        // A foreign repo slug on the same API host is rejected.
        assert!(require_release_repository(
            "https://api.github.com/repos/attacker/evil/releases/assets/42"
        )
        .is_err());
        // A non-asset api.github.com path for the pinned repo is also rejected —
        // only the release-asset endpoint is accepted, nothing else.
        assert!(require_release_repository(
            "https://api.github.com/repos/ovm-sh/ovm-oss/git/blobs/deadbeef"
        )
        .is_err());
    }

    #[test]
    fn token_authenticates_metadata_and_uses_asset_api() {
        let temp = tempdir().unwrap();
        let archive_path = temp.path().join("release.tar.gz");
        let manifest = "ovm-bundle-v1\nmain\tovm\tovm\nside\tovm-side\tovm-side\n";
        write_archive(&archive_path, manifest, &["ovm", "ovm-side"], None);
        let archive_bytes = std::fs::read(&archive_path).unwrap();
        let digest = hex_digest(Sha256::digest(&archive_bytes));
        let target = target_triple(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let archive_name = format!("ovm-{target}.tar.gz");

        let mut server = Server::new();
        // browser_download_url is present but points at github.com; with a token
        // it must NOT be used (no mock backs it and the repo-pin would reject
        // the smuggled path), proving the asset-API path is taken instead.
        let metadata = serde_json::json!({
            "tag_name": "v0.1.0",
            "draft": false,
            "prerelease": false,
            "assets": [
                {"id": 11, "name": archive_name, "browser_download_url": "https://github.com/unused"},
                {"id": 22, "name": format!("{archive_name}.sha256"), "browser_download_url": "https://github.com/unused"}
            ]
        });
        let metadata_mock = server
            .mock("GET", "/repos/ovm-sh/ovm-oss/releases/latest")
            .match_header("authorization", "Bearer secret-token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(metadata.to_string())
            .create();
        let archive_mock = server
            .mock("GET", "/repos/ovm-sh/ovm-oss/releases/assets/11")
            .match_header("authorization", "Bearer secret-token")
            .match_header("accept", "application/octet-stream")
            .with_status(200)
            .with_body(archive_bytes)
            .create();
        let checksum_mock = server
            .mock("GET", "/repos/ovm-sh/ovm-oss/releases/assets/22")
            .match_header("authorization", "Bearer secret-token")
            .match_header("accept", "application/octet-stream")
            .with_status(200)
            .with_body(format!("{digest}  {archive_name}\n"))
            .create();

        let bundle = prepare_from_api(
            SelfUpdateChannel::Stable,
            &server.url(),
            Some("secret-token"),
        )
        .unwrap();
        assert_eq!(bundle.release.version, "0.1.0");
        assert!(bundle.source_dir.join("ovm-side").is_file());
        metadata_mock.assert();
        archive_mock.assert();
        checksum_mock.assert();
    }

    #[test]
    fn download_strips_bearer_on_cross_host_redirect() {
        // The signed CDN target lives on a different port, so reqwest must drop
        // the Authorization header when it follows the asset-API redirect there.
        let mut cdn = Server::new();
        let signed = cdn
            .mock("GET", "/signed-object")
            .match_header("authorization", mockito::Matcher::Missing)
            .with_status(200)
            .with_body(b"payload-bytes")
            .create();

        let mut api = Server::new();
        let signed_location = format!("{}/signed-object", cdn.url());
        let asset = api
            .mock("GET", "/repos/ovm-sh/ovm-oss/releases/assets/7")
            .match_header("authorization", "Bearer secret-token")
            .with_status(302)
            .with_header("location", &signed_location)
            .create();

        let temp = tempdir().unwrap();
        let out = temp.path().join("asset.bin");
        let url = format!("{}/repos/ovm-sh/ovm-oss/releases/assets/7", api.url());
        download(&url, &out, Some("secret-token"), true).unwrap();

        asset.assert();
        signed.assert();
        assert_eq!(std::fs::read(&out).unwrap(), b"payload-bytes");
    }
}
