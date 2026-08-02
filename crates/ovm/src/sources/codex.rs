use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::release_metadata::ReleaseInstallMetadata;
use console::style;
use serde::Deserialize;
use sha2::{Digest, Sha512};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const DEFAULT_RELEASES_API_BASE: &str = "https://api.github.com/repos/openai/codex/releases";
const CODEX_NPM_REGISTRY_URL: &str = "https://registry.npmjs.org/@openai/codex";

/// Helper binaries that newer Codex releases ship alongside `codex` and spawn
/// at runtime from the same directory (0.144.0 introduced
/// `codex-code-mode-host`, without which every shell command fails to spawn).
/// Older releases don't publish them, so a missing asset/entry is skipped
/// rather than treated as an error.
const SIDECAR_BINARIES: &[&str] = &["codex-code-mode-host"];
const RELEASE_METADATA_TIMEOUT_SECS: u64 = 30;
const RELEASE_ASSET_TIMEOUT_SECS: u64 = 300;
const NPM_METADATA_TIMEOUT_SECS: u64 = 15;
const NPM_ASSET_TIMEOUT_SECS: u64 = 300;

/// Resolve the Codex releases API URL. Tests set `OVM_CODEX_RELEASES_URL` to a mock server.
fn releases_api_base() -> String {
    std::env::var("OVM_CODEX_RELEASES_URL")
        .unwrap_or_else(|_| DEFAULT_RELEASES_API_BASE.to_string())
}

fn npm_registry_url() -> String {
    std::env::var("OVM_CODEX_NPM_REGISTRY_URL")
        .unwrap_or_else(|_| CODEX_NPM_REGISTRY_URL.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    /// Byte size the releases API records for this asset. It is a second,
    /// independently-fetched declaration of how long the body must be, so a
    /// download that ends early is caught even if the CDN sends no
    /// `Content-Length`.
    ///
    /// Optional here so that *listing* releases survives a payload we don't
    /// fully understand; the download path insists on it via
    /// [`declared_asset_size`], because there a missing size silently reduces
    /// verification rather than failing.
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct NpmPackageInfo {
    #[serde(rename = "dist-tags")]
    dist_tags: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct NpmVersionInfo {
    dist: NpmDistInfo,
}

#[derive(Debug, Deserialize)]
struct NpmDistInfo {
    tarball: String,
    #[serde(default)]
    integrity: Option<String>,
}

pub fn get_latest_version() -> Result<String> {
    if let Ok(version) = get_latest_npm_release_version() {
        return Ok(version);
    }

    let release = fetch_release("latest")?;
    if is_installable_codex_release(&release)
        && Product::Codex.is_release_version(&release.tag_name)
    {
        return Ok(release.tag_name);
    }

    latest_release_version(list_remote_versions()?)
        .ok_or_else(|| OvmError::VersionNotFound("latest".into()))
}

pub fn get_latest_npm_release_version() -> Result<String> {
    get_latest_npm_release_version_at(&npm_registry_url())
}

fn get_latest_npm_release_version_at(url: &str) -> Result<String> {
    let response = npm_metadata_client()?
        .get(url)
        .header("Accept", "application/json")
        .send()?;

    if !response.status().is_success() {
        return Err(OvmError::DownloadFailed {
            url: url.to_string(),
            message: format!("HTTP {}", response.status()),
        });
    }

    let info: NpmPackageInfo = response.json()?;
    let latest = info
        .dist_tags
        .get("latest")
        .ok_or_else(|| OvmError::VersionNotFound("latest".into()))?;
    let version = semver::Version::parse(latest)
        .map_err(|_| OvmError::VersionNotFound(latest.to_string()))?;
    if !version.pre.is_empty() {
        return Err(OvmError::VersionNotFound(latest.to_string()));
    }

    Ok(format!("rust-v{version}"))
}

pub fn list_remote_versions() -> Result<Vec<String>> {
    list_remote_versions_at(&releases_api_base())
}

fn list_remote_versions_at(api_url: &str) -> Result<Vec<String>> {
    let client = release_metadata_client()?;
    let mut release_tags = Vec::new();
    let mut page = 1_u32;

    loop {
        let response = client
            .get(api_url)
            .query(&[("per_page", 100_u32), ("page", page)])
            .send()?;

        if !response.status().is_success() {
            return Err(OvmError::DownloadFailed {
                url: api_url.to_string(),
                message: format!("HTTP {}", response.status()),
            });
        }

        let releases: Vec<Release> = response.json()?;
        if releases.is_empty() {
            break;
        }
        release_tags.extend(
            releases
                .into_iter()
                .filter(is_installable_codex_release)
                .map(|r| r.tag_name),
        );
        page += 1;
    }

    Ok(release_tags)
}

fn is_installable_codex_release(release: &Release) -> bool {
    Product::Codex.is_official_remote_version(&release.tag_name)
        && select_release_asset(release).is_some()
}

fn latest_release_version(versions: Vec<String>) -> Option<String> {
    versions
        .into_iter()
        .filter(|version| {
            Product::Codex.is_official_remote_version(version)
                && Product::Codex.is_release_version(version)
        })
        .max_by(|left, right| Product::Codex.compare_version_strings(left, right))
}

pub fn download_release(version: &str, dest: &Path) -> Result<ReleaseInstallMetadata> {
    // Fetched once and used twice: the GitHub path needs the asset list, and
    // the npm fallback needs to know whether this version publishes sidecars at
    // all. Fetching it separately per path would let the two disagree about the
    // same release.
    let fetched = fetch_release(version);
    let requirement = match &fetched {
        Ok(release) => SidecarRequirement::from_release(release),
        Err(error) => SidecarRequirement::Unknown(error.to_string()),
    };

    let github_error = match fetched {
        Ok(release) => {
            // Before anything is downloaded, and OUTSIDE the fallback chain: a
            // release that publishes the sidecar family for other platforms and
            // not for ours is known-broken here. No other registry can
            // republish an asset that was never built, so falling through to
            // npm would only route around this refusal — which is exactly how
            // the 0.144.0 shape would reach a user again.
            refuse_incomplete_sidecar_family(&release, version)?;
            match install_github_release(&release, version, dest) {
                Ok(metadata) => return complete_install(metadata, &requirement, version, dest),
                Err(error) => error,
            }
        }
        Err(error) => error,
    };

    match download_npm_release(version, dest) {
        Ok(metadata) => complete_install(metadata, &requirement, version, dest),
        Err(npm_error) => Err(OvmError::Message(format!(
            "Could not download Codex {version} from GitHub releases ({github_error}) or npm ({npm_error})"
        ))),
    }
}

/// The GitHub path alone, with no npm fallback wrapped around it. Only tests
/// use it: they need a GitHub-path failure to surface as itself rather than as
/// the combined "GitHub … or npm …" message.
#[cfg(test)]
fn download_github_release(version: &str, dest: &Path) -> Result<ReleaseInstallMetadata> {
    let release = fetch_release(version)?;
    install_github_release(&release, version, dest)
}

fn install_github_release(
    release: &Release,
    version: &str,
    dest: &Path,
) -> Result<ReleaseInstallMetadata> {
    let asset = select_release_asset(release).ok_or_else(|| OvmError::DownloadFailed {
        url: format!("{}/tags/{version}", releases_api_base()),
        message: format!(
            "No supported asset found for any of: {}",
            expected_asset_names().join(", ")
        ),
    })?;

    crate::util::ensure_parent_dir(dest)?;

    let archive_path = dest.with_extension("tar.gz");
    let asset_name = asset.name.clone();
    let asset_url = asset.browser_download_url.clone();
    let asset_size = declared_asset_size(asset)?;
    let archive_sha256 =
        download_and_extract_single_binary(&asset_url, &archive_path, dest, Some(asset_size))?;
    if let Err(error) = super::verify_product_binary(Product::Codex, dest) {
        let _ = std::fs::remove_file(dest);
        return Err(error);
    }
    install_github_sidecars(release, version, dest)?;
    Ok(ReleaseInstallMetadata::new(
        version,
        release.tag_name.clone(),
        asset_name,
        asset_url,
        archive_sha256,
    ))
}

fn download_npm_release(version: &str, dest: &Path) -> Result<ReleaseInstallMetadata> {
    if !Product::Codex.is_official_remote_version(version) {
        return Err(OvmError::VersionNotFound(version.to_string()));
    }

    let npm_version = codex_npm_platform_version(version)?;
    let metadata_url = format!("{}/{npm_version}", npm_registry_url());
    let response = npm_metadata_client()?
        .get(&metadata_url)
        .header("Accept", "application/json")
        .send()?;

    if !response.status().is_success() {
        return Err(OvmError::VersionNotFound(npm_version));
    }

    let info: NpmVersionInfo = response.json()?;
    let tarball_url = info.dist.tarball;
    let allow_loopback = super::test_override_active("OVM_CODEX_NPM_REGISTRY_URL");
    super::validate_download_url(&tarball_url, &["registry.npmjs.org"], allow_loopback)?;

    crate::util::ensure_parent_dir(dest)?;
    let archive_path = dest.with_extension("npm.tgz");
    let download_result =
        download_npm_tarball(&tarball_url, info.dist.integrity.as_deref(), &archive_path);
    let extract_result =
        download_result.and_then(|sha256| extract_npm_archive(&archive_path, dest).map(|_| sha256));
    let _ = std::fs::remove_file(&archive_path);
    let archive_sha256 = extract_result?;

    for binary in installed_binary_paths(dest) {
        if let Err(error) = super::verify_product_binary(Product::Codex, &binary) {
            for installed in installed_binary_paths(dest) {
                let _ = std::fs::remove_file(installed);
            }
            return Err(error);
        }
    }

    Ok(ReleaseInstallMetadata::new(
        version,
        format!("npm:{npm_version}"),
        format!("@openai/codex@{npm_version}"),
        tarball_url,
        archive_sha256,
    ))
}

fn fetch_release(version: &str) -> Result<Release> {
    let path = if version == "latest" {
        "latest".to_string()
    } else {
        format!("tags/{version}")
    };
    let url = format!("{}/{path}", releases_api_base());
    let response = release_metadata_client()?.get(&url).send()?;

    if !response.status().is_success() {
        return Err(OvmError::VersionNotFound(version.to_string()));
    }

    Ok(response.json()?)
}

fn release_metadata_client() -> Result<reqwest::blocking::Client> {
    super::http_client(RELEASE_METADATA_TIMEOUT_SECS)
}

fn release_asset_client() -> Result<reqwest::blocking::Client> {
    super::download_http_client(RELEASE_ASSET_TIMEOUT_SECS, super::GITHUB_DOWNLOAD_HOSTS)
}

fn npm_metadata_client() -> Result<reqwest::blocking::Client> {
    super::http_client(NPM_METADATA_TIMEOUT_SECS)
}

fn npm_asset_client() -> Result<reqwest::blocking::Client> {
    super::download_http_client(NPM_ASSET_TIMEOUT_SECS, &["registry.npmjs.org"])
}

fn codex_npm_platform_version(version: &str) -> Result<String> {
    let Some(base_version) = version.strip_prefix("rust-v") else {
        return Err(OvmError::VersionNotFound(version.to_string()));
    };

    Ok(format!("{base_version}-{}", npm_platform_suffix()?))
}

fn npm_platform_suffix() -> Result<&'static str> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Ok("darwin-arm64");
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        return Ok("darwin-x64");
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        return Ok("linux-arm64");
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return Ok("linux-x64");
    }
    #[allow(unreachable_code)]
    Err(OvmError::Message(
        "No Codex npm platform package is available for this platform.".into(),
    ))
}

fn expected_asset_names() -> &'static [&'static str] {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        &[
            "codex-aarch64-apple-darwin.tar.gz",
            "codex-aarch64-apple-darwin-unsigned.tar.gz",
        ]
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        &[
            "codex-x86_64-apple-darwin.tar.gz",
            "codex-x86_64-apple-darwin-unsigned.tar.gz",
        ]
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        &["codex-aarch64-unknown-linux-musl.tar.gz"]
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        &["codex-x86_64-unknown-linux-musl.tar.gz"]
    }
}

/// The byte size the releases API records for an asset we are about to
/// download. Required, not optional: GitHub declares a size for every asset,
/// so a missing one means the metadata is not the shape we believe it is —
/// and quietly dropping to a `Content-Length`-only check would render that
/// unknown as a normal install. The [`ReleaseAsset::size`] field stays
/// optional because *listing* releases must survive a payload surprise; only
/// the download path, where the number is load-bearing, insists on it.
pub(crate) fn declared_asset_size(asset: &ReleaseAsset) -> Result<u64> {
    asset.size.ok_or_else(|| OvmError::DownloadFailed {
        url: asset.browser_download_url.clone(),
        message: format!(
            "the release metadata declares no size for {}, so the downloaded length cannot be \
             checked against a second source",
            asset.name
        ),
    })
}

fn select_release_asset(release: &Release) -> Option<&ReleaseAsset> {
    expected_asset_names()
        .iter()
        .find_map(|expected| release.assets.iter().find(|asset| asset.name == *expected))
}

fn download_asset(url: &str, dest: &Path, metadata_size: Option<u64>) -> Result<String> {
    // Loopback is only legitimate when the Codex releases test override points at
    // a local mock; production release metadata must never resolve to loopback.
    let allow_loopback = super::test_override_active("OVM_CODEX_RELEASES_URL");
    super::validate_download_url(url, super::GITHUB_DOWNLOAD_HOSTS, allow_loopback)?;
    let mut response = release_asset_client()?.get(url).send()?;
    super::validate_download_url(
        response.url().as_str(),
        super::GITHUB_DOWNLOAD_HOSTS,
        allow_loopback,
    )?;

    if !response.status().is_success() {
        return Err(OvmError::DownloadFailed {
            url: url.to_string(),
            message: format!("HTTP {}", response.status()),
        });
    }

    // Read before the body is consumed. No transparent decompression is
    // enabled on the client, so this is the exact number of bytes to expect.
    let content_length = response.content_length();
    let result = super::stream_to_file(&mut response, dest, url, "release asset", None).and_then(
        |(sha256, downloaded)| {
            super::validate_downloaded_size(url, downloaded, content_length, metadata_size)?;
            Ok(sha256)
        },
    );
    if result.is_err() {
        // Never leave a short archive on disk to be mistaken for a good one.
        let _ = std::fs::remove_file(dest);
    }
    result
}

fn download_npm_tarball(url: &str, integrity: Option<&str>, dest: &Path) -> Result<String> {
    let allow_loopback = super::test_override_active("OVM_CODEX_NPM_REGISTRY_URL");
    let mut response = npm_asset_client()?.get(url).send()?;
    super::validate_download_url(
        response.url().as_str(),
        &["registry.npmjs.org"],
        allow_loopback,
    )?;

    if !response.status().is_success() {
        return Err(OvmError::DownloadFailed {
            url: url.to_string(),
            message: format!("HTTP {}", response.status()),
        });
    }

    let content_length = response.content_length();
    let mut sha512 = Sha512::new();
    let result = super::stream_to_file(&mut response, dest, url, "npm tarball", Some(&mut sha512))
        .and_then(|(sha256, downloaded)| {
            super::validate_downloaded_size(url, downloaded, content_length, None)?;
            if let Some(integrity) = integrity {
                crate::sources::npm::verify_sha512_integrity(integrity, &sha512.finalize())?;
            }
            Ok(sha256)
        });
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

/// Download a single-binary release asset and extract it to `dest`, removing
/// the downloaded archive whether or not extraction succeeds. Returns the
/// archive's sha256.
fn download_and_extract_single_binary(
    url: &str,
    archive_path: &Path,
    dest: &Path,
    metadata_size: Option<u64>,
) -> Result<String> {
    let download_result = download_asset(url, archive_path, metadata_size);
    let result = download_result
        .and_then(|sha256| extract_release_archive(archive_path, dest).map(|_| sha256));
    let _ = std::fs::remove_file(archive_path);
    result
}

/// Whether `release` publishes `sidecar` for *some* platform — i.e. its asset
/// list holds at least one member of that sidecar's family
/// (`codex-code-mode-host-<triple>.tar.gz`, and any signature/checksum files
/// beside it). The main binary's assets (`codex-<triple>.tar.gz`) never match,
/// because the prefix tested is the full sidecar name plus `-`.
fn release_publishes_sidecar_family(release: &Release, sidecar: &str) -> bool {
    let prefix = format!("{sidecar}-");
    release
        .assets
        .iter()
        .any(|asset| asset.name.starts_with(&prefix))
}

/// Which runtime sidecars a version is known to require.
///
/// The only honest signal OVM has is the GitHub release's own asset list: if a
/// release publishes `codex-code-mode-host-<triple>.tar.gz` for any platform,
/// that version's Codex spawns the sidecar, and an install without it cannot
/// run a shell command. This is the same evidence the `--strict-sidecars`
/// release-radar probe uses (the release asset manifest). npm publishes no
/// per-file listing we could ask instead, so the knowledge is derived once from
/// the release and carried into whichever source ends up producing the tree.
enum SidecarRequirement {
    /// The release listing was read. These sidecars are published for at least
    /// one platform; empty means a release from before the sidecar existed.
    Published(Vec<&'static str>),
    /// The release listing could not be read at all, so requirement is
    /// undecidable. Mirrors the probe's `inconclusive` sidecar status, which
    /// passes by default: refusing every install whenever the releases API is
    /// rate-limited would disable the npm fallback exactly when it is needed.
    /// It is said out loud rather than assumed away (see
    /// [`complete_install`]).
    Unknown(String),
}

impl SidecarRequirement {
    fn from_release(release: &Release) -> Self {
        Self::Published(
            SIDECAR_BINARIES
                .iter()
                .copied()
                .filter(|sidecar| release_publishes_sidecar_family(release, sidecar))
                .collect(),
        )
    }
}

/// The guarantee, checked where it is finally made: whatever source produced
/// the tree — GitHub asset, npm platform package — a version whose release
/// publishes a sidecar family must have that sidecar on disk before the install
/// is handed back as good.
///
/// The GitHub path enforces this asset-by-asset already; the npm path could
/// not, because its extractor only sees whatever the package happens to
/// contain. Checking here means the promise holds across the whole install
/// rather than on one path.
fn complete_install(
    metadata: ReleaseInstallMetadata,
    requirement: &SidecarRequirement,
    version: &str,
    dest: &Path,
) -> Result<ReleaseInstallMetadata> {
    let Some(bin_dir) = dest.parent() else {
        return Ok(metadata);
    };
    for sidecar in SIDECAR_BINARIES {
        if bin_dir.join(sidecar).exists() {
            continue;
        }
        match requirement {
            SidecarRequirement::Published(published) if published.contains(sidecar) => {
                for installed in installed_binary_paths(dest) {
                    let _ = std::fs::remove_file(installed);
                }
                return Err(OvmError::DownloadFailed {
                    url: format!("{}/tags/{version}", releases_api_base()),
                    message: format!(
                        "Codex {version} ships the {sidecar} sidecar, but the package installed \
                         here contains no {sidecar} beside the Codex binary. Installing it would \
                         leave a Codex that cannot run shell commands, so nothing was installed. \
                         Try another version, or report this at \
                         https://github.com/ovm-sh/ovm-oss/issues."
                    ),
                });
            }
            SidecarRequirement::Published(_) => {}
            SidecarRequirement::Unknown(reason) => eprintln!(
                "  {} Installed Codex {version} without {sidecar}, and could not read the release \
                 metadata to tell whether this version needs it ({reason}). If shell commands \
                 fail to spawn, reinstall once the GitHub releases API is reachable.",
                style("!").yellow()
            ),
        }
    }
    Ok(metadata)
}

/// Refuse a release that publishes a sidecar family for other platforms but not
/// for ours — the 0.144.0 shape, where a Codex installs looking healthy and
/// then cannot spawn a single shell command.
///
/// Releases from before the sidecar existed publish none at all, and those keep
/// installing. This is the one condition both the pre-download refusal in
/// [`download_release`] and [`install_github_sidecars`] consult, so the two can
/// never drift into disagreeing about the same release.
fn refuse_incomplete_sidecar_family(release: &Release, version: &str) -> Result<()> {
    for sidecar in SIDECAR_BINARIES {
        let asset_name = format!("{sidecar}-{}.tar.gz", release_target_triple());
        if release.assets.iter().any(|asset| asset.name == asset_name) {
            continue;
        }
        if !release_publishes_sidecar_family(release, sidecar) {
            continue;
        }
        return Err(OvmError::DownloadFailed {
            url: format!("{}/tags/{version}", releases_api_base()),
            message: format!(
                "Codex {} publishes {sidecar} for other platforms but not {asset_name}. \
                 Installing it would leave a Codex that cannot run shell commands, so \
                 nothing was installed. Try another version, or report this at \
                 https://github.com/ovm-sh/ovm-oss/issues.",
                release.tag_name
            ),
        });
    }
    Ok(())
}

/// Install the [`SIDECAR_BINARIES`] that this release publishes as separate
/// assets (e.g. `codex-code-mode-host-aarch64-apple-darwin.tar.gz`) next to
/// the main binary at `dest`. A sidecar that exists and fails to install is an
/// error, for the same reason one that is missing for our platform alone is.
fn install_github_sidecars(release: &Release, version: &str, dest: &Path) -> Result<()> {
    let Some(bin_dir) = dest.parent() else {
        return Ok(());
    };
    if let Err(error) = refuse_incomplete_sidecar_family(release, version) {
        let _ = std::fs::remove_file(dest);
        return Err(error);
    }
    for sidecar in SIDECAR_BINARIES {
        let asset_name = format!("{sidecar}-{}.tar.gz", release_target_triple());
        let Some(asset) = release.assets.iter().find(|asset| asset.name == asset_name) else {
            continue;
        };
        let sidecar_dest = bin_dir.join(sidecar);
        let archive_path = sidecar_dest.with_extension("tar.gz");
        let install_result = declared_asset_size(asset)
            .and_then(|size| {
                download_and_extract_single_binary(
                    &asset.browser_download_url,
                    &archive_path,
                    &sidecar_dest,
                    Some(size),
                )
            })
            .and_then(|_| super::verify_product_binary(Product::Codex, &sidecar_dest));
        if let Err(error) = install_result {
            let _ = std::fs::remove_file(&sidecar_dest);
            let _ = std::fs::remove_file(dest);
            return Err(error);
        }
    }
    Ok(())
}

fn release_target_triple() -> &'static str {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "aarch64-apple-darwin"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "x86_64-apple-darwin"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "aarch64-unknown-linux-musl"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "x86_64-unknown-linux-musl"
    }
}

/// The main binary plus any sidecars that are present next to it.
fn installed_binary_paths(dest: &Path) -> Vec<PathBuf> {
    let mut paths = vec![dest.to_path_buf()];
    if let Some(bin_dir) = dest.parent() {
        for sidecar in SIDECAR_BINARIES {
            let path = bin_dir.join(sidecar);
            if path.exists() {
                paths.push(path);
            }
        }
    }
    paths
}

/// Extract the Codex binaries from an npm platform tarball: the entry named
/// exactly `codex` becomes `dest`, and any [`SIDECAR_BINARIES`] entries are
/// installed next to it. Other vendored files (rg, zsh, …) are skipped — the
/// CLI treats those as optional and falls back to system tools.
fn extract_npm_archive(archive_path: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive_path)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);

    let parent = dest
        .parent()
        .ok_or_else(|| OvmError::Config(format!("No parent directory for {}", dest.display())))?;
    std::fs::create_dir_all(parent)?;

    let temp_dir = tempfile::tempdir_in(parent)?;
    let mut staged: Vec<(String, PathBuf)> = Vec::new();

    for entry in archive.entries()? {
        let mut entry =
            entry.map_err(|error| archive_read_error(error, "unreadable archive entry"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }

        let entry_path = entry
            .path()
            .map_err(|error| archive_read_error(error, "unreadable archive entry path"))?;
        let Some(file_name) = entry_path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
        else {
            continue;
        };

        if file_name != "codex" && !SIDECAR_BINARIES.contains(&file_name.as_str()) {
            continue;
        }

        // Use the effective (PAX-aware) size, not the raw header size: a PAX
        // `size` extended header overrides the header and drives how many bytes
        // `unpack` streams, so validating the header alone could be bypassed.
        let declared_size = entry.size();
        crate::sources::validate_tar_entry_size(declared_size, std::path::Path::new(&file_name))?;

        let staged_path = temp_dir.path().join(&file_name);
        entry
            .unpack(&staged_path)
            .map_err(|error| archive_read_error(error, &format!("failed to unpack {file_name}")))?;
        crate::util::make_executable(&staged_path)?;
        staged.push((file_name, staged_path));
    }

    if !staged.iter().any(|(file_name, _)| file_name == "codex") {
        return Err(OvmError::ExtractionFailed(
            "the npm package unpacked completely but contains no Codex binary".into(),
        ));
    }

    // Commit only after the whole archive has been read successfully, so a
    // truncated/corrupt tarball never leaves partial binaries in the bin dir
    // (an existing bin path makes later installs treat the version as
    // already installed). Roll back everything if a rename fails partway.
    let mut installed: Vec<PathBuf> = Vec::new();
    for (file_name, staged_path) in &staged {
        let target = if file_name == "codex" {
            dest.to_path_buf()
        } else {
            parent.join(file_name)
        };
        if let Err(error) = std::fs::rename(staged_path, &target) {
            for path in &installed {
                let _ = std::fs::remove_file(path);
            }
            return Err(error.into());
        }
        installed.push(target);
    }

    Ok(())
}

/// Classify an error raised while reading a downloaded archive.
///
/// A short read (`UnexpectedEof` from the gzip/tar layer) means the bytes on
/// disk are not a whole archive — a truncated download, not a bad release. The
/// two cases used to produce the same message, so an interrupted transfer read
/// as "the publisher shipped a broken archive".
pub(crate) fn archive_read_error(error: std::io::Error, context: &str) -> OvmError {
    if error.kind() == std::io::ErrorKind::UnexpectedEof {
        return OvmError::ExtractionFailed(format!(
            "archive incomplete or truncated (interrupted download?): {error}. Retry the install."
        ));
    }
    OvmError::ExtractionFailed(format!("{context}: {error}"))
}

fn extract_release_archive(archive_path: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive_path)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);

    let parent = dest
        .parent()
        .ok_or_else(|| OvmError::Config(format!("No parent directory for {}", dest.display())))?;
    std::fs::create_dir_all(parent)?;

    let temp_dir = tempfile::tempdir_in(parent)?;
    let mut extracted = false;

    for entry in archive.entries()? {
        let mut entry =
            entry.map_err(|error| archive_read_error(error, "unreadable archive entry"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }

        let entry_path = entry
            .path()
            .map_err(|error| archive_read_error(error, "unreadable archive entry path"))?;
        let Some(file_name) = entry_path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
        else {
            continue;
        };

        if !is_codex_binary_name(&file_name) {
            continue;
        }

        // Effective (PAX-aware) size, not the raw header size (see above).
        let declared_size = entry.size();
        crate::sources::validate_tar_entry_size(declared_size, std::path::Path::new(&file_name))?;

        let extracted_path = temp_dir.path().join(&file_name);
        entry
            .unpack(&extracted_path)
            .map_err(|error| archive_read_error(error, &format!("failed to unpack {file_name}")))?;
        // Permissions go on the staged file, before it is published. Chmod'ing
        // the published path afterwards left a window where `dest` existed
        // non-executable (a concurrent `ovm run` would see a "cannot execute"
        // binary), and chmod-by-path follows links. The npm extractor above
        // already stages-then-publishes this way.
        crate::util::make_executable(&extracted_path)?;
        std::fs::rename(&extracted_path, dest)?;
        extracted = true;
        break;
    }

    if !extracted {
        // The archive read to completion (a truncated one fails above with the
        // incomplete-archive error), so this really is a release-shape problem.
        return Err(OvmError::ExtractionFailed(
            "the release archive unpacked completely but contains no Codex binary".into(),
        ));
    }

    Ok(())
}

fn is_codex_binary_name(file_name: &str) -> bool {
    file_name == "codex" || file_name.starts_with("codex-")
}

#[cfg(test)]
mod tests {
    use super::{
        codex_npm_platform_version, declared_asset_size, download_and_extract_single_binary,
        download_github_release, download_release, expected_asset_names, extract_npm_archive,
        extract_release_archive, get_latest_npm_release_version_at, install_github_sidecars,
        latest_release_version, list_remote_versions_at, release_target_triple,
        select_release_asset, Release, ReleaseAsset,
    };
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use mockito::Server;
    use tar::Builder;
    use tempfile::tempdir;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn asset(name: &str, url: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_string(),
            browser_download_url: url.to_string(),
            size: None,
        }
    }

    #[test]
    fn list_remote_versions_paginates() {
        let mut server = Server::new();
        let asset = expected_asset_names()[0];
        let fallback_asset = expected_asset_names().last().copied().unwrap_or(asset);
        let _p1 = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::UrlEncoded("page".into(), "1".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"[
                    {{"tag_name":"rust-v0.120.0","assets":[{{"name":"{asset}","browser_download_url":"https://example.com/codex.tar.gz"}}]}},
                    {{"tag_name":"rust-v0.119.0","assets":[{{"name":"{fallback_asset}","browser_download_url":"https://example.com/codex.tar.gz"}}]}},
                    {{"tag_name":"rusty-v8-v147.4.0","assets":[{{"name":"{asset}","browser_download_url":"https://example.com/codex.tar.gz"}}]}},
                    {{"tag_name":"codex-rs-deadbeef-1-rust-v0.0.2504301219","assets":[{{"name":"{asset}","browser_download_url":"https://example.com/codex.tar.gz"}}]}},
                    {{"tag_name":"rust-v0.117.0","assets":[{{"name":"other-platform.tar.gz","browser_download_url":"https://example.com/other.tar.gz"}}]}}
                ]"#
            ))
            .create();
        let _p2 = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::UrlEncoded("page".into(), "2".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"[{{"tag_name":"rust-v0.118.0","assets":[{{"name":"{asset}","browser_download_url":"https://example.com/codex.tar.gz"}}]}}]"#
            ))
            .create();
        let _p3 = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::UrlEncoded("page".into(), "3".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .create();

        let versions = list_remote_versions_at(&server.url()).expect("success");
        assert_eq!(
            versions,
            vec!["rust-v0.120.0", "rust-v0.119.0", "rust-v0.118.0"]
        );
    }

    #[test]
    fn list_remote_versions_errors_on_5xx() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(503)
            .create();
        let result = list_remote_versions_at(&server.url());
        assert!(result.is_err());
    }

    #[test]
    fn latest_release_version_ignores_prereleases() {
        let latest = latest_release_version(vec![
            "rust-v0.130.0".into(),
            "rust-v0.131.0-alpha.16".into(),
            "rust-v0.129.0".into(),
        ]);

        assert_eq!(latest.as_deref(), Some("rust-v0.130.0"));
    }

    #[test]
    fn npm_latest_dist_tag_maps_to_rust_release_tag() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"dist-tags":{"latest":"0.142.0","alpha":"0.143.0-alpha.6"}}"#)
            .create();

        let latest = get_latest_npm_release_version_at(&server.url()).expect("latest");

        assert_eq!(latest, "rust-v0.142.0");
    }

    #[test]
    fn npm_platform_version_uses_rust_release_tag() {
        let version =
            codex_npm_platform_version("rust-v0.142.0").expect("supported platform version");

        assert!(version.starts_with("0.142.0-"));
        assert!(!version.starts_with("rust-v"));
    }

    #[test]
    fn select_release_asset_prefers_expected_order_over_api_order() {
        let expected = expected_asset_names();
        let mut assets: Vec<ReleaseAsset> = expected
            .iter()
            .rev()
            .map(|name| asset(name, &format!("https://example.com/{name}")))
            .collect();
        assets.insert(
            0,
            asset("other-platform.tar.gz", "https://example.com/other"),
        );
        let release = Release {
            tag_name: "rust-v0.120.0".into(),
            assets,
        };

        let selected = select_release_asset(&release).expect("asset selected");
        assert_eq!(selected.name, expected[0]);
    }

    #[test]
    fn extracts_platform_named_codex_binary_to_destination() {
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("codex.tar.gz");
        let dest = dir.path().join("release").join("bin").join("codex");

        create_archive(
            &archive_path,
            "codex-aarch64-apple-darwin",
            b"fake-codex-binary",
        );

        extract_release_archive(&archive_path, &dest).expect("extract archive");

        assert_eq!(
            std::fs::read(&dest).expect("read extracted binary"),
            b"fake-codex-binary"
        );
    }

    fn create_archive(path: &std::path::Path, entry_name: &str, contents: &[u8]) {
        create_multi_archive(path, &[(entry_name, contents)]);
    }

    fn create_multi_archive(path: &std::path::Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).expect("create archive");
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);

        for (entry_name, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, entry_name, *contents)
                .expect("append archive entry");
        }

        let encoder = builder.into_inner().expect("finish tar");
        encoder.finish().expect("finish gzip");
    }

    /// A gzipped tar holding a single entry whose name, mode and entry type are
    /// all attacker-chosen. The safe-archive tests below need to build shapes a
    /// legitimate release never produces.
    fn create_hostile_archive(
        path: &std::path::Path,
        entry_name: &str,
        mode: u32,
        entry_type: tar::EntryType,
        link_target: Option<&str>,
        contents: &[u8],
    ) {
        let file = std::fs::File::create(path).expect("create archive");
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);

        let mut header = tar::Header::new_gnu();
        header.set_mode(mode);
        header.set_entry_type(entry_type);
        match link_target {
            Some(target) => {
                header.set_size(0);
                builder
                    .append_link(&mut header, entry_name, target)
                    .expect("append link entry");
            }
            None => {
                header.set_size(contents.len() as u64);
                // The name goes straight into the header field rather than
                // through `set_path`/`append_data`, both of which refuse to
                // WRITE a `..` component. A hostile publisher has no such
                // scruples, and the point of these tests is what OVM does when
                // it READS one.
                {
                    let name_field = &mut header.as_old_mut().name;
                    let bytes = entry_name.as_bytes();
                    assert!(
                        bytes.len() < name_field.len(),
                        "entry name too long for a v7 header"
                    );
                    name_field[..bytes.len()].copy_from_slice(bytes);
                }
                header.set_cksum();
                builder
                    .append(&header, contents)
                    .expect("append archive entry");
            }
        }

        let encoder = builder.into_inner().expect("finish tar");
        encoder.finish().expect("finish gzip");
    }

    #[test]
    fn release_archive_entry_escaping_its_directory_cannot_write_outside_the_destination() {
        // A release asset is fetched over the network from a third party. If a
        // `../` in an entry name were honoured, unpacking would write anywhere
        // the user can write — the classic tar-slip. Extraction must use only
        // the entry's final path component, so the payload lands at `dest` and
        // the traversal has no effect at all.
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("slip.tar.gz");
        let sandbox = dir.path().join("sandbox");
        let dest = sandbox.join("release").join("bin").join("codex");

        // A pre-existing file exactly where the traversal aims. Asserting only
        // "nothing new appeared there" is not enough: extraction unpacks to a
        // scratch path and then renames, so an escaped write would be moved
        // away again and leave no trace. A victim file that must survive
        // untouched catches both the write and the move.
        let escape_target = dir.path().join("codex");
        std::fs::write(
            &escape_target,
            b"pre-existing file the archive must not touch",
        )
        .expect("seed the traversal victim");

        create_hostile_archive(
            &archive_path,
            "../../../../codex",
            0o755,
            tar::EntryType::Regular,
            None,
            b"payload",
        );

        extract_release_archive(&archive_path, &dest).expect("traversal is neutralised, not fatal");

        assert_eq!(
            std::fs::read(&dest).expect("read extracted binary"),
            b"payload"
        );
        assert_eq!(
            std::fs::read(&escape_target).expect("the traversal victim must still exist"),
            b"pre-existing file the archive must not touch",
            "a `../` entry name wrote outside the destination directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn release_archive_symlink_named_like_the_binary_is_not_installed() {
        // A symlink entry called `codex` pointing at a file the user can read
        // would turn `ovm run codex` into "execute whatever that path is", and
        // would let a later write through the link clobber an arbitrary file.
        // Only regular files may become the installed binary.
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("symlink.tar.gz");
        let dest = dir.path().join("release").join("bin").join("codex");

        create_hostile_archive(
            &archive_path,
            "codex",
            0o777,
            tar::EntryType::Symlink,
            Some("/etc/passwd"),
            b"",
        );

        let error = extract_release_archive(&archive_path, &dest)
            .expect_err("a symlink must not satisfy the binary entry");
        assert!(error.to_string().contains("no Codex binary"), "{error}");
        assert!(
            !dest.exists() && std::fs::symlink_metadata(&dest).is_err(),
            "a symlink entry was installed as the Codex binary"
        );
    }

    #[cfg(unix)]
    #[test]
    fn release_archive_setuid_bits_are_stripped_from_the_installed_binary() {
        // tar preserves header modes, so a setuid-root archive entry would be
        // written setuid. The installed binary must end up exactly 0o755 —
        // asserting `mode & 0o755 == 0o755` (the shape this suite used to use)
        // passes for 0o4777 and proves nothing.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("setuid.tar.gz");
        let dest = dir.path().join("release").join("bin").join("codex");

        create_hostile_archive(
            &archive_path,
            "codex",
            0o4777,
            tar::EntryType::Regular,
            None,
            b"payload",
        );

        extract_release_archive(&archive_path, &dest).expect("extract archive");

        let mode = std::fs::metadata(&dest)
            .expect("stat extracted binary")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            mode, 0o755,
            "installed binary mode is {mode:o}, expected exactly 755 (setuid/setgid/world-write must not survive)"
        );
    }

    #[test]
    fn release_archive_rejects_oversized_entry() {
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("bomb.tar.gz");
        let dest = dir.path().join("release").join("bin").join("codex");

        // A `codex` entry whose header claims ~8 GiB (above the 4 GiB cap) with
        // no data behind it. Extraction must reject on the declared size before
        // reading or writing the entry.
        create_tar_gz_with_declared_size(&archive_path, b"codex", 0o77777777777);

        let error =
            extract_release_archive(&archive_path, &dest).expect_err("oversized entry rejected");
        assert!(error.to_string().contains("oversized"), "{error}");
        assert!(!dest.exists());
    }

    #[test]
    fn npm_archive_rejects_oversized_entry() {
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("bomb.npm.tgz");
        let dest = dir.path().join("release").join("bin").join("codex");

        create_tar_gz_with_declared_size(
            &archive_path,
            b"package/vendor/aarch64-apple-darwin/bin/codex",
            0o77777777777,
        );

        let error =
            extract_npm_archive(&archive_path, &dest).expect_err("oversized entry rejected");
        assert!(error.to_string().contains("oversized"), "{error}");
        assert!(!dest.exists());
    }

    /// Write a tar.gz whose single regular-file entry *declares* `declared_size`
    /// bytes while carrying no data — exercises the size cap without a huge file.
    fn create_tar_gz_with_declared_size(
        path: &std::path::Path,
        entry_name: &[u8],
        declared_size: u64,
    ) {
        use std::io::Write;

        let mut header = [0u8; 512];
        let len = entry_name.len().min(99);
        header[..len].copy_from_slice(&entry_name[..len]);
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
        let file = std::fs::File::create(path).expect("create file");
        let mut gz = GzEncoder::new(file, Compression::default());
        gz.write_all(&header).expect("write header");
        gz.write_all(&end).expect("write end");
        gz.finish().expect("finish gzip");
    }

    #[test]
    fn npm_archive_extracts_main_binary_and_sidecars() {
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("codex.npm.tgz");
        let dest = dir.path().join("release").join("bin").join("codex");

        // Sidecar listed before the main binary: extraction must match names
        // exactly, not take the first `codex*` entry as the main binary.
        create_multi_archive(
            &archive_path,
            &[
                (
                    "package/vendor/aarch64-apple-darwin/bin/codex-code-mode-host",
                    b"fake-host-binary".as_slice(),
                ),
                (
                    "package/vendor/aarch64-apple-darwin/bin/codex",
                    b"fake-codex-binary".as_slice(),
                ),
                (
                    "package/vendor/aarch64-apple-darwin/codex-path/rg",
                    b"fake-rg-binary".as_slice(),
                ),
            ],
        );

        extract_npm_archive(&archive_path, &dest).expect("extract archive");

        assert_eq!(
            std::fs::read(&dest).expect("read main binary"),
            b"fake-codex-binary"
        );
        let bin_dir = dest.parent().expect("bin dir");
        assert_eq!(
            std::fs::read(bin_dir.join("codex-code-mode-host")).expect("read sidecar"),
            b"fake-host-binary"
        );
        assert!(!bin_dir.join("rg").exists());
    }

    #[test]
    fn npm_archive_without_sidecar_still_installs_main_binary() {
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("codex.npm.tgz");
        let dest = dir.path().join("release").join("bin").join("codex");

        create_multi_archive(
            &archive_path,
            &[(
                "package/vendor/aarch64-apple-darwin/bin/codex",
                b"fake-codex-binary".as_slice(),
            )],
        );

        extract_npm_archive(&archive_path, &dest).expect("extract archive");

        assert_eq!(
            std::fs::read(&dest).expect("read main binary"),
            b"fake-codex-binary"
        );
        assert!(!dest
            .parent()
            .expect("bin dir")
            .join("codex-code-mode-host")
            .exists());
    }

    #[test]
    fn truncated_npm_archive_installs_nothing() {
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("codex.npm.tgz");
        let dest = dir.path().join("release").join("bin").join("codex");

        // Main binary first so a naive streaming extraction would have
        // already installed it by the time the truncation is hit.
        create_multi_archive(
            &archive_path,
            &[
                (
                    "package/vendor/aarch64-apple-darwin/bin/codex",
                    vec![0xAB; 64 * 1024].as_slice(),
                ),
                (
                    "package/vendor/aarch64-apple-darwin/bin/codex-code-mode-host",
                    vec![0xCD; 64 * 1024].as_slice(),
                ),
            ],
        );
        let bytes = std::fs::read(&archive_path).expect("read archive");
        std::fs::write(&archive_path, &bytes[..bytes.len() / 2]).expect("truncate archive");

        let result = extract_npm_archive(&archive_path, &dest);

        assert!(result.is_err(), "truncated archive should fail extraction");
        assert!(!dest.exists(), "main binary must not be committed");
        assert!(
            !dest
                .parent()
                .expect("bin dir")
                .join("codex-code-mode-host")
                .exists(),
            "sidecar must not be committed"
        );
    }

    // ── Download integrity ───────────────────────────────────────────

    /// Serve `body` with NO `Content-Length`, then close. This is the framing
    /// where an interrupted transfer is invisible: the client sees a clean EOF
    /// and cannot tell a short body from a complete one. (With a
    /// `Content-Length` present, hyper itself rejects a short body.)
    fn serve_close_delimited(body: Vec<u8>) -> u16 {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n");
                let _ = stream.write_all(&body);
            }
        });
        port
    }

    /// The regression this guards: a download cut short used to be stored and
    /// hashed as if complete, and the failure only surfaced at extraction as
    /// "no Codex binary in the release archive" — blaming the publisher for
    /// our own short read.
    #[test]
    fn a_truncated_download_is_reported_as_incomplete_not_as_a_bad_release() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("codex.tar.gz");
        let dest = dir.path().join("bin").join("codex");
        create_archive(&archive_path, "codex", b"fake-codex-binary");
        let complete = std::fs::read(&archive_path).expect("read archive");
        std::fs::remove_file(&archive_path).expect("remove staged archive");

        let port = serve_close_delimited(complete[..complete.len() / 2].to_vec());
        std::env::set_var("OVM_CODEX_RELEASES_URL", format!("http://127.0.0.1:{port}"));
        let error = download_and_extract_single_binary(
            &format!("http://127.0.0.1:{port}/codex.tar.gz"),
            &archive_path,
            &dest,
            Some(complete.len() as u64),
        )
        .expect_err("a truncated download must fail");
        std::env::remove_var("OVM_CODEX_RELEASES_URL");

        let message = error.to_string();
        assert!(message.contains("incomplete download"), "{message}");
        assert!(
            !message.contains("no Codex binary"),
            "a short read must not be blamed on the release: {message}"
        );
        assert!(!dest.exists());
        assert!(
            !archive_path.exists(),
            "a short archive must not be left on disk"
        );
    }

    /// Serve a Codex release whose single platform asset carries `size_field`
    /// verbatim (e.g. `,"size":123` or ``). Returns (server, archive bytes).
    fn codex_release_server(
        size_field: &str,
        body: Vec<u8>,
    ) -> (mockito::ServerGuard, Vec<mockito::Mock>) {
        let mut server = Server::new();
        let asset_name = expected_asset_names()[0];
        let base = server.url();
        let release_json = format!(
            r#"{{"tag_name":"rust-v0.130.0","assets":[{{"name":"{asset_name}","browser_download_url":"{base}/assets/{asset_name}"{size_field}}}]}}"#
        );
        let mocks = vec![
            server
                .mock("GET", "/tags/rust-v0.130.0")
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(release_json)
                .create(),
            server
                .mock("GET", format!("/assets/{asset_name}").as_str())
                .with_status(200)
                .with_header("content-type", "application/octet-stream")
                .with_body(body)
                .create(),
        ];
        (server, mocks)
    }

    fn codex_archive_bytes(dir: &std::path::Path) -> Vec<u8> {
        let archive_path = dir.join("staged.tar.gz");
        create_archive(&archive_path, "codex", b"#!/bin/sh\nexit 0\n");
        let bytes = std::fs::read(&archive_path).expect("read archive");
        std::fs::remove_file(&archive_path).expect("remove staged archive");
        bytes
    }

    /// GitHub declares a size for every release asset, and on the GitHub path
    /// that number is one of only two integrity signals we have. Metadata that
    /// carries none is metadata we do not understand — it must fail loudly
    /// rather than quietly reduce verification to `Content-Length` alone.
    #[test]
    fn an_asset_without_a_declared_size_fails_the_github_download() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempdir().expect("tempdir");
        let (server, _mocks) = codex_release_server("", codex_archive_bytes(dir.path()));

        std::env::set_var("OVM_CODEX_RELEASES_URL", server.url());
        let dest = dir.path().join("bin").join("codex");
        let result = download_github_release("rust-v0.130.0", &dest);
        std::env::remove_var("OVM_CODEX_RELEASES_URL");

        let error = result.expect_err("an undeclared asset size must not install");
        assert!(error.to_string().contains("declares no size"), "{error}");
        assert!(!dest.exists());
    }

    /// The same release with the size GitHub actually publishes gets past the
    /// gate, so the check above is a distinction and not a blanket refusal.
    /// (An unsigned fake binary is then rejected by the signature check on
    /// macOS — that is a later, different gate.)
    #[test]
    fn an_asset_with_a_declared_size_passes_the_size_gate() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempdir().expect("tempdir");
        let body = codex_archive_bytes(dir.path());
        let (server, _mocks) = codex_release_server(&format!(r#","size":{}"#, body.len()), body);

        std::env::set_var("OVM_CODEX_RELEASES_URL", server.url());
        let dest = dir.path().join("bin").join("codex");
        let result = download_github_release("rust-v0.130.0", &dest);
        std::env::remove_var("OVM_CODEX_RELEASES_URL");

        if let Err(error) = result {
            assert!(
                !error.to_string().contains("declares no size"),
                "a sized asset must clear the declared-size gate: {error}"
            );
        }
    }

    #[test]
    fn declared_asset_size_reports_absence_as_a_metadata_failure() {
        let sized = ReleaseAsset {
            size: Some(42),
            ..asset("codex.tar.gz", "https://example.com/codex.tar.gz")
        };
        assert_eq!(declared_asset_size(&sized).expect("declared size"), 42);

        let error = declared_asset_size(&asset("codex.tar.gz", "https://example.com/codex.tar.gz"))
            .expect_err("no declared size");
        assert!(error.to_string().contains("declares no size"), "{error}");
    }

    /// An archive that arrived whole but genuinely has no `codex` entry keeps
    /// blaming the release — the two causes must stay distinguishable.
    #[test]
    fn a_complete_archive_without_the_binary_still_blames_the_release() {
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("codex.tar.gz");
        let dest = dir.path().join("bin").join("codex");
        create_archive(&archive_path, "README.md", b"no binary here");

        let error = extract_release_archive(&archive_path, &dest).expect_err("no binary");
        let message = error.to_string();
        assert!(message.contains("no Codex binary"), "{message}");
        assert!(!message.contains("truncated"), "{message}");
    }

    /// A truncated archive on disk reads as truncated, not as a release that
    /// forgot to ship its binary.
    #[test]
    fn a_truncated_archive_reads_as_truncated() {
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("codex.tar.gz");
        let dest = dir.path().join("bin").join("codex");
        create_archive(&archive_path, "codex", &vec![0xAB; 64 * 1024]);
        let bytes = std::fs::read(&archive_path).expect("read archive");
        std::fs::write(&archive_path, &bytes[..bytes.len() / 2]).expect("truncate");

        let error = extract_release_archive(&archive_path, &dest).expect_err("truncated");
        let message = error.to_string();
        assert!(
            message.contains("incomplete or truncated"),
            "a truncated archive must say so: {message}"
        );
        assert!(!message.contains("no Codex binary"), "{message}");
    }

    // ── Sidecar completeness ─────────────────────────────────────────

    /// A release that ships the sidecar for other platforms but not for ours is
    /// the 0.144.0 shape: the install used to `continue` past it and publish a
    /// Codex whose shell commands cannot spawn. It must fail, naming the asset.
    #[test]
    fn a_release_missing_only_our_sidecar_fails_the_install() {
        let dir = tempdir().expect("tempdir");
        let dest = dir.path().join("bin").join("codex");
        std::fs::create_dir_all(dest.parent().expect("bin dir")).expect("bin dir");
        std::fs::write(&dest, b"fake-codex-binary").expect("main binary");

        // Every platform's sidecar except this one's.
        let ours = format!(
            "codex-code-mode-host-{}.tar.gz",
            super::release_target_triple()
        );
        let assets = [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "aarch64-unknown-linux-musl",
            "x86_64-unknown-linux-musl",
        ]
        .iter()
        .map(|triple| format!("codex-code-mode-host-{triple}.tar.gz"))
        .filter(|name| *name != ours)
        .map(|name| asset(&name, &format!("https://example.com/{name}")))
        .collect();
        let release = Release {
            tag_name: "rust-v0.144.0".into(),
            assets,
        };

        let error = install_github_sidecars(&release, "rust-v0.144.0", &dest)
            .expect_err("a platform-missing sidecar must fail the install");
        let message = error.to_string();
        assert!(
            message.contains(&ours),
            "the missing asset must be named: {message}"
        );
        assert!(
            message.contains("cannot run shell commands"),
            "the consequence must be stated: {message}"
        );
        assert!(!dest.exists(), "no half-installed Codex may be left behind");
    }

    /// The other direction: releases from before the sidecar existed publish no
    /// sidecar asset for any platform, and must keep installing.
    #[test]
    fn a_release_without_any_sidecar_still_installs() {
        let dir = tempdir().expect("tempdir");
        let dest = dir.path().join("bin").join("codex");
        std::fs::create_dir_all(dest.parent().expect("bin dir")).expect("bin dir");
        std::fs::write(&dest, b"fake-codex-binary").expect("main binary");

        let release = Release {
            tag_name: "rust-v0.130.0".into(),
            assets: expected_asset_names()
                .iter()
                .map(|name| asset(name, &format!("https://example.com/{name}")))
                .collect(),
        };

        install_github_sidecars(&release, "rust-v0.130.0", &dest)
            .expect("a sidecar-free release still installs");
        assert!(dest.exists(), "the main binary must survive");
    }

    // ── Sidecar completeness across BOTH sources ─────────────────────

    const ALL_TRIPLES: &[&str] = &[
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "aarch64-unknown-linux-musl",
        "x86_64-unknown-linux-musl",
    ];

    /// Every triple except the one we are running on.
    fn other_triples() -> Vec<&'static str> {
        ALL_TRIPLES
            .iter()
            .copied()
            .filter(|triple| *triple != release_target_triple())
            .collect()
    }

    fn tarball_bytes(dir: &std::path::Path, name: &str, entries: &[(&str, &[u8])]) -> Vec<u8> {
        let path = dir.join(name);
        create_multi_archive(&path, entries);
        let bytes = std::fs::read(&path).expect("read archive");
        std::fs::remove_file(&path).expect("remove staged archive");
        bytes
    }

    struct MockSources {
        server: mockito::ServerGuard,
        mocks: Vec<mockito::Mock>,
        npm_metadata: mockito::Mock,
    }

    /// One mock server standing in for BOTH Codex sources: the GitHub releases
    /// API and the npm registry. That pairing is the point — these tests are
    /// about what the *whole install* does when the two disagree about what a
    /// version ships.
    ///
    /// `sidecar_triples` are the platforms whose sidecar asset the release
    /// publishes; `main_asset_ok` false makes the platform tarball 500 so the
    /// install falls through to npm; `npm_has_sidecar` says whether the npm
    /// platform package carries the sidecar binary.
    fn mock_sources(
        dir: &std::path::Path,
        version: &str,
        sidecar_triples: &[&str],
        main_asset_ok: bool,
        npm_has_sidecar: bool,
        npm_expected_hits: usize,
    ) -> MockSources {
        let mut server = Server::new();
        let base = server.url();
        let main_asset = expected_asset_names()[0];
        let main_body = tarball_bytes(dir, "main.tar.gz", &[("codex", b"fake-codex-binary")]);
        let sidecar_body = tarball_bytes(
            dir,
            "sidecar.tar.gz",
            &[("codex-code-mode-host", b"fake-host-binary")],
        );

        let mut assets = vec![format!(
            r#"{{"name":"{main_asset}","browser_download_url":"{base}/assets/{main_asset}","size":{}}}"#,
            main_body.len()
        )];
        let mut mocks = vec![server
            .mock("GET", format!("/assets/{main_asset}").as_str())
            .with_status(if main_asset_ok { 200 } else { 500 })
            .with_header("content-type", "application/octet-stream")
            .with_body(if main_asset_ok {
                main_body.clone()
            } else {
                Vec::new()
            })
            .create()];
        for triple in sidecar_triples {
            let name = format!("codex-code-mode-host-{triple}.tar.gz");
            assets.push(format!(
                r#"{{"name":"{name}","browser_download_url":"{base}/assets/{name}","size":{}}}"#,
                sidecar_body.len()
            ));
            mocks.push(
                server
                    .mock("GET", format!("/assets/{name}").as_str())
                    .with_status(200)
                    .with_header("content-type", "application/octet-stream")
                    .with_body(sidecar_body.clone())
                    .create(),
            );
        }
        mocks.push(
            server
                .mock("GET", format!("/tags/{version}").as_str())
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(format!(
                    r#"{{"tag_name":"{version}","assets":[{}]}}"#,
                    assets.join(",")
                ))
                .create(),
        );

        let mut npm_entries: Vec<(&str, &[u8])> =
            vec![("package/vendor/bin/codex", b"fake-codex-binary")];
        if npm_has_sidecar {
            npm_entries.push((
                "package/vendor/bin/codex-code-mode-host",
                b"fake-host-binary",
            ));
        }
        let npm_body = tarball_bytes(dir, "npm.tgz", &npm_entries);
        mocks.push(
            server
                .mock("GET", "/npm/package.tgz")
                .with_status(200)
                .with_header("content-type", "application/octet-stream")
                .with_body(npm_body)
                .create(),
        );
        let npm_version = codex_npm_platform_version(version).expect("npm platform version");
        let npm_metadata = server
            .mock("GET", format!("/{npm_version}").as_str())
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"dist":{{"tarball":"{base}/npm/package.tgz"}}}}"#
            ))
            .expect(npm_expected_hits)
            .create();

        MockSources {
            server,
            mocks,
            npm_metadata,
        }
    }

    /// Point both source overrides at `base` and switch signature verification
    /// off (the fixtures are unsigned), restoring the environment afterwards.
    fn with_mock_sources<T>(base: &str, body: impl FnOnce() -> T) -> T {
        let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let _signature = crate::sources::SIGNATURE_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::env::set_var("OVM_CODEX_RELEASES_URL", base);
        std::env::set_var("OVM_CODEX_NPM_REGISTRY_URL", base);
        std::env::set_var("OVM_SKIP_SIGNATURE_VERIFY", "1");
        let result = body();
        std::env::remove_var("OVM_CODEX_RELEASES_URL");
        std::env::remove_var("OVM_CODEX_NPM_REGISTRY_URL");
        std::env::remove_var("OVM_SKIP_SIGNATURE_VERIFY");
        result
    }

    /// The bypass this finding is about: the GitHub path correctly refuses a
    /// release that ships the sidecar family for other platforms and not for
    /// ours — and then the npm fallback used to be tried anyway. npm here would
    /// happily serve a package, so only a refusal that skips the fallback
    /// entirely can keep 0.144.0 from reaching a user through the back door.
    #[test]
    fn a_release_missing_our_sidecar_never_falls_back_to_npm() {
        let dir = tempdir().expect("tempdir");
        let sources = mock_sources(dir.path(), "rust-v0.144.0", &other_triples(), true, true, 0);
        let dest = dir.path().join("bin").join("codex");

        let result = with_mock_sources(&sources.server.url(), || {
            download_release("rust-v0.144.0", &dest)
        });

        let error = result.expect_err("a platform-missing sidecar must fail the whole install");
        let message = error.to_string();
        assert!(
            message.contains(&format!(
                "codex-code-mode-host-{}.tar.gz",
                release_target_triple()
            )),
            "the missing asset must be named: {message}"
        );
        assert!(
            message.contains("cannot run shell commands"),
            "the consequence must be stated: {message}"
        );
        assert!(
            !message.contains("Could not download Codex"),
            "the refusal must not be reported as a two-source download failure: {message}"
        );
        assert!(!dest.exists(), "nothing may be installed");
        assert!(
            !dest.parent().expect("bin dir").join("codex").exists(),
            "nothing may be installed"
        );
        // The npm registry must not have been consulted at all.
        sources.npm_metadata.assert();
        drop(sources.mocks);
    }

    /// The npm fallback held to the same standard: this version's release
    /// publishes the sidecar family, so a platform package that carries only
    /// the main binary produces exactly the broken install the GitHub check
    /// exists to prevent.
    #[test]
    fn an_npm_package_missing_a_required_sidecar_is_refused() {
        let dir = tempdir().expect("tempdir");
        let sources = mock_sources(dir.path(), "rust-v0.144.0", ALL_TRIPLES, false, false, 1);
        let dest = dir.path().join("bin").join("codex");

        let result = with_mock_sources(&sources.server.url(), || {
            download_release("rust-v0.144.0", &dest)
        });

        let error = result.expect_err("an npm package without a required sidecar must not install");
        let message = error.to_string();
        assert!(
            message.contains("codex-code-mode-host"),
            "the missing sidecar must be named: {message}"
        );
        assert!(
            message.contains("cannot run shell commands"),
            "the consequence must be stated: {message}"
        );
        assert!(!dest.exists(), "no half-installed Codex may be left behind");
        sources.npm_metadata.assert();
        drop(sources.mocks);
    }

    /// The other direction on the npm path: the same version, from a package
    /// that does carry the sidecar, installs both binaries.
    #[test]
    fn an_npm_package_with_the_required_sidecar_installs() {
        let dir = tempdir().expect("tempdir");
        let sources = mock_sources(dir.path(), "rust-v0.144.0", ALL_TRIPLES, false, true, 1);
        let dest = dir.path().join("bin").join("codex");

        let result = with_mock_sources(&sources.server.url(), || {
            download_release("rust-v0.144.0", &dest)
        });

        result.expect("a complete npm package installs");
        assert_eq!(
            std::fs::read(&dest).expect("read main binary"),
            b"fake-codex-binary"
        );
        assert_eq!(
            std::fs::read(dest.parent().expect("bin dir").join("codex-code-mode-host"))
                .expect("read sidecar"),
            b"fake-host-binary"
        );
        drop(sources.mocks);
    }

    /// And the other direction for the requirement itself: a release from
    /// before the sidecar existed publishes none for any platform, so an npm
    /// package without one is complete and must keep installing.
    #[test]
    fn an_npm_package_without_a_sidecar_installs_when_the_release_ships_none() {
        let dir = tempdir().expect("tempdir");
        let sources = mock_sources(dir.path(), "rust-v0.130.0", &[], false, false, 1);
        let dest = dir.path().join("bin").join("codex");

        let result = with_mock_sources(&sources.server.url(), || {
            download_release("rust-v0.130.0", &dest)
        });

        result.expect("a sidecar-free release still installs from npm");
        assert_eq!(
            std::fs::read(&dest).expect("read main binary"),
            b"fake-codex-binary"
        );
        assert!(!dest
            .parent()
            .expect("bin dir")
            .join("codex-code-mode-host")
            .exists());
        drop(sources.mocks);
    }

    #[test]
    fn npm_archive_without_main_binary_fails_and_cleans_up_sidecars() {
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("codex.npm.tgz");
        let dest = dir.path().join("release").join("bin").join("codex");

        create_multi_archive(
            &archive_path,
            &[(
                "package/vendor/aarch64-apple-darwin/bin/codex-code-mode-host",
                b"fake-host-binary".as_slice(),
            )],
        );

        let result = extract_npm_archive(&archive_path, &dest);

        assert!(result.is_err());
        assert!(!dest.exists());
        assert!(!dest
            .parent()
            .expect("bin dir")
            .join("codex-code-mode-host")
            .exists());
    }
}
