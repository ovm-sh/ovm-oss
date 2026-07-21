use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::release_metadata::ReleaseInstallMetadata;
use serde::Deserialize;
use sha2::{Digest, Sha256, Sha512};
use std::collections::HashMap;
use std::io::{self, Read};
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
    match download_github_release(version, dest) {
        Ok(metadata) => Ok(metadata),
        Err(github_error) => match download_npm_release(version, dest) {
            Ok(metadata) => Ok(metadata),
            Err(npm_error) => Err(OvmError::Message(format!(
                "Could not download Codex {version} from GitHub releases ({github_error}) or npm ({npm_error})"
            ))),
        },
    }
}

fn download_github_release(version: &str, dest: &Path) -> Result<ReleaseInstallMetadata> {
    let release = fetch_release(version)?;
    let asset = select_release_asset(&release).ok_or_else(|| OvmError::DownloadFailed {
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
    let archive_sha256 = download_and_extract_single_binary(&asset_url, &archive_path, dest)?;
    if let Err(error) = super::verify_product_binary(Product::Codex, dest) {
        let _ = std::fs::remove_file(dest);
        return Err(error);
    }
    install_github_sidecars(&release, dest)?;
    Ok(ReleaseInstallMetadata::new(
        version,
        release.tag_name,
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

fn select_release_asset(release: &Release) -> Option<&ReleaseAsset> {
    expected_asset_names()
        .iter()
        .find_map(|expected| release.assets.iter().find(|asset| asset.name == *expected))
}

fn download_asset(url: &str, dest: &Path) -> Result<String> {
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

    let mut file = std::fs::File::create(dest)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];

    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|error| OvmError::DownloadFailed {
                url: url.to_string(),
                message: format!("failed to read release asset body: {}", error),
            })?;
        if read == 0 {
            break;
        }

        io::Write::write_all(&mut file, &buffer[..read]).map_err(|error| {
            OvmError::DownloadFailed {
                url: url.to_string(),
                message: format!(
                    "failed to write release asset to {}: {}",
                    dest.display(),
                    error
                ),
            }
        })?;
        hasher.update(&buffer[..read]);
    }

    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
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

    let mut file = std::fs::File::create(dest)?;
    let mut sha256 = Sha256::new();
    let mut sha512 = Sha512::new();
    let mut buffer = [0u8; 8192];

    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|error| OvmError::DownloadFailed {
                url: url.to_string(),
                message: format!("failed to read npm tarball body: {error}"),
            })?;
        if read == 0 {
            break;
        }

        io::Write::write_all(&mut file, &buffer[..read]).map_err(|error| {
            OvmError::DownloadFailed {
                url: url.to_string(),
                message: format!("failed to write npm tarball to {}: {error}", dest.display()),
            }
        })?;
        sha256.update(&buffer[..read]);
        sha512.update(&buffer[..read]);
    }

    if let Some(integrity) = integrity {
        if let Err(error) =
            crate::sources::npm::verify_sha512_integrity(integrity, &sha512.finalize())
        {
            let _ = std::fs::remove_file(dest);
            return Err(error);
        }
    }

    Ok(sha256
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Download a single-binary release asset and extract it to `dest`, removing
/// the downloaded archive whether or not extraction succeeds. Returns the
/// archive's sha256.
fn download_and_extract_single_binary(
    url: &str,
    archive_path: &Path,
    dest: &Path,
) -> Result<String> {
    let download_result = download_asset(url, archive_path);
    let result = download_result
        .and_then(|sha256| extract_release_archive(archive_path, dest).map(|_| sha256));
    let _ = std::fs::remove_file(archive_path);
    result
}

/// Install the [`SIDECAR_BINARIES`] that this release publishes as separate
/// assets (e.g. `codex-code-mode-host-aarch64-apple-darwin.tar.gz`) next to
/// the main binary at `dest`. Releases that don't publish a sidecar asset are
/// left as-is; a sidecar that exists but fails to install is an error, since
/// the CLI cannot run commands without it.
fn install_github_sidecars(release: &Release, dest: &Path) -> Result<()> {
    let Some(bin_dir) = dest.parent() else {
        return Ok(());
    };
    for sidecar in SIDECAR_BINARIES {
        let asset_name = format!("{sidecar}-{}.tar.gz", release_target_triple());
        let Some(asset) = release.assets.iter().find(|asset| asset.name == asset_name) else {
            continue;
        };
        let sidecar_dest = bin_dir.join(sidecar);
        let archive_path = sidecar_dest.with_extension("tar.gz");
        let install_result = download_and_extract_single_binary(
            &asset.browser_download_url,
            &archive_path,
            &sidecar_dest,
        )
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
        let mut entry = entry.map_err(|error| OvmError::ExtractionFailed(error.to_string()))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }

        let entry_path = entry
            .path()
            .map_err(|error| OvmError::ExtractionFailed(error.to_string()))?;
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
            .map_err(|error| OvmError::ExtractionFailed(error.to_string()))?;
        crate::util::make_executable(&staged_path)?;
        staged.push((file_name, staged_path));
    }

    if !staged.iter().any(|(file_name, _)| file_name == "codex") {
        return Err(OvmError::ExtractionFailed(
            "Could not find Codex binary in npm package".into(),
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
        let mut entry = entry.map_err(|error| OvmError::ExtractionFailed(error.to_string()))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }

        let entry_path = entry
            .path()
            .map_err(|error| OvmError::ExtractionFailed(error.to_string()))?;
        let Some(file_name) = entry_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        if !is_codex_binary_name(file_name) {
            continue;
        }

        // Effective (PAX-aware) size, not the raw header size (see above).
        let declared_size = entry.size();
        crate::sources::validate_tar_entry_size(declared_size, std::path::Path::new(file_name))?;

        let extracted_path = temp_dir.path().join(file_name);
        entry
            .unpack(&extracted_path)
            .map_err(|error| OvmError::ExtractionFailed(error.to_string()))?;
        std::fs::rename(&extracted_path, dest)?;
        extracted = true;
        break;
    }

    if !extracted {
        return Err(OvmError::ExtractionFailed(
            "Could not find Codex binary in release archive".into(),
        ));
    }

    crate::util::make_executable(dest)?;

    Ok(())
}

fn is_codex_binary_name(file_name: &str) -> bool {
    file_name == "codex" || file_name.starts_with("codex-")
}

#[cfg(test)]
mod tests {
    use super::{
        codex_npm_platform_version, expected_asset_names, extract_npm_archive,
        extract_release_archive, get_latest_npm_release_version_at, latest_release_version,
        list_remote_versions_at, select_release_asset, Release, ReleaseAsset,
    };
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use mockito::Server;
    use tar::Builder;
    use tempfile::tempdir;

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
            .map(|name| ReleaseAsset {
                name: (*name).to_string(),
                browser_download_url: format!("https://example.com/{name}"),
            })
            .collect();
        assets.insert(
            0,
            ReleaseAsset {
                name: "other-platform.tar.gz".into(),
                browser_download_url: "https://example.com/other".into(),
            },
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
