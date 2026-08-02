use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::release_metadata::ReleaseInstallMetadata;
use crate::sources::codex::Release;
use console::style;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

const DEFAULT_RELEASES_API_BASE: &str = "https://api.github.com/repos/earendil-works/pi/releases";
const PI_NPM_REGISTRY_URL: &str = "https://registry.npmjs.org/@earendil-works/pi-coding-agent";
const RELEASE_METADATA_TIMEOUT_SECS: u64 = 30;
const RELEASE_ASSET_TIMEOUT_SECS: u64 = 300;
const NPM_METADATA_TIMEOUT_SECS: u64 = 15;

/// Resolve the Pi releases API URL. Tests set `OVM_PI_RELEASES_URL` to a mock server.
fn releases_api_base() -> String {
    std::env::var("OVM_PI_RELEASES_URL").unwrap_or_else(|_| DEFAULT_RELEASES_API_BASE.to_string())
}

fn npm_registry_url() -> String {
    std::env::var("OVM_PI_NPM_REGISTRY_URL").unwrap_or_else(|_| PI_NPM_REGISTRY_URL.to_string())
}

#[derive(Debug, Deserialize)]
struct NpmPackageInfo {
    #[serde(rename = "dist-tags")]
    dist_tags: HashMap<String, String>,
}

pub fn get_latest_version() -> Result<String> {
    if let Ok(version) = get_latest_npm_release_version() {
        return Ok(version);
    }

    Ok(Product::Pi.normalize_version(&fetch_release("latest")?.tag_name))
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

    Ok(version.to_string())
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
                .map(|r| Product::Pi.normalize_version(&r.tag_name)),
        );
        page += 1;
    }

    Ok(release_tags)
}

/// Download and extract the Pi release bundle.
/// `bundle_dir` should be the directory where the bundle is extracted (e.g., `release/bundle`).
/// The binary will be at `bundle_dir/pi/pi`.
pub fn download_release(version: &str, bundle_dir: &Path) -> Result<ReleaseInstallMetadata> {
    let tag = format_tag(version);
    // A release-metadata failure (GitHub rate limit, 5xx) is survivable — the
    // asset URL is predictable — but it must never pass unmentioned, and it
    // must not decide on its own that this release has no checksum. Say it out
    // loud, then ask the download host directly (see `published_checksum`).
    let release = match fetch_release(&tag) {
        Ok(release) => Some(release),
        Err(error) => {
            eprintln!(
                "  {} Could not read the Pi release metadata for {tag} ({error}); \
                 falling back to the published asset URL",
                style("!").yellow()
            );
            None
        }
    };
    let platform_asset = release.as_ref().and_then(|release| {
        release
            .assets
            .iter()
            .find(|asset| asset.name == expected_asset_name())
    });
    let (resolved_tag, asset_name, download_url, metadata_size) =
        match (release.as_ref(), platform_asset) {
            (Some(release), Some(asset)) => (
                release.tag_name.clone(),
                asset.name.clone(),
                asset.browser_download_url.clone(),
                Some(crate::sources::codex::declared_asset_size(asset)?),
            ),
            _ => {
                let (tag, name, url) = direct_release_asset(&tag);
                (tag, name, url, None)
            }
        };
    // Pi publishes a SHA256SUMS asset alongside its tarballs (newer releases
    // only). When it is there the archive is verified against it; when the
    // release genuinely publishes none, the declared-length checks are the
    // whole integrity story. "We could not read the digest" is neither of
    // those, and must not be served as the second one.
    let expected_sha256 = match published_checksum(release.as_ref(), &tag, &asset_name) {
        PublishedChecksum::Digest(digest) => Some(digest),
        PublishedChecksum::NotPublished => None,
        PublishedChecksum::Unavailable(reason) => {
            return Err(OvmError::DownloadFailed {
                url: download_url,
                message: format!(
                    "this release publishes a {CHECKSUMS_ASSET} manifest but it could not be \
                     verified ({reason}); refusing to install on length checks alone"
                ),
            })
        }
    };

    std::fs::create_dir_all(bundle_dir)?;

    let archive_path = bundle_dir.join("pi.tar.gz");
    let archive_sha256 = download_asset(&download_url, &archive_path, metadata_size)?;
    if let Some(expected) = &expected_sha256 {
        if !archive_sha256.eq_ignore_ascii_case(expected) {
            let _ = std::fs::remove_file(&archive_path);
            return Err(OvmError::DownloadFailed {
                url: download_url,
                message: format!(
                    "checksum mismatch against the release's SHA256SUMS: expected {expected}, got {archive_sha256}"
                ),
            });
        }
    }
    let extract_result = extract_full_archive(&archive_path, bundle_dir);
    let _ = std::fs::remove_file(&archive_path);
    extract_result?;

    // Verify the binary exists
    let binary = bundle_dir.join("pi").join("pi");
    if !binary.exists() {
        // A truncated archive fails in `extract_full_archive` above with the
        // incomplete-archive error, so reaching here means the bundle really
        // was published without the binary at the expected path.
        return Err(OvmError::ExtractionFailed(
            "the Pi bundle unpacked completely but contains no pi/pi binary".into(),
        ));
    }
    crate::util::make_executable(&binary)?;

    Ok(ReleaseInstallMetadata::new(
        version,
        resolved_tag,
        asset_name,
        download_url,
        archive_sha256,
    ))
}

fn format_tag(version: &str) -> String {
    if version == "latest" || version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    }
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

fn npm_metadata_client() -> Result<reqwest::blocking::Client> {
    super::http_client(NPM_METADATA_TIMEOUT_SECS)
}

fn release_asset_client() -> Result<reqwest::blocking::Client> {
    super::download_http_client(RELEASE_ASSET_TIMEOUT_SECS, super::GITHUB_DOWNLOAD_HOSTS)
}

fn expected_asset_name() -> &'static str {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "pi-darwin-arm64.tar.gz"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "pi-darwin-x64.tar.gz"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "pi-linux-arm64.tar.gz"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "pi-linux-x64.tar.gz"
    }
}

fn direct_release_asset_url(tag: &str, file_name: &str) -> String {
    if releases_api_base() == DEFAULT_RELEASES_API_BASE {
        format!("https://github.com/earendil-works/pi/releases/download/{tag}/{file_name}")
    } else {
        format!(
            "{}/download/{tag}/{file_name}",
            releases_api_base().trim_end_matches('/')
        )
    }
}

fn direct_release_download_url(tag: &str) -> String {
    direct_release_asset_url(tag, expected_asset_name())
}

fn direct_release_asset(tag: &str) -> (String, String, String) {
    (
        tag.to_string(),
        expected_asset_name().to_string(),
        direct_release_download_url(tag),
    )
}

/// Name of the digest manifest Pi publishes with each release.
const CHECKSUMS_ASSET: &str = "SHA256SUMS";

/// Cap on the checksum manifest body — it is a few hundred bytes in practice.
const MAX_CHECKSUMS_BYTES: u64 = 1024 * 1024;

/// What a release says about the digest of the archive we are about to install.
///
/// Three states, because two cannot hold them. Collapsing them into
/// `Option<String>` made every failure — a rate-limited API, a 500 on the
/// manifest, a truncated body, a parse error — arrive as `None`, which the
/// installer reads as "this release publishes no checksum" and proceeds on
/// length alone. That is our own blindness rendered as a clean verdict.
#[derive(Debug, PartialEq, Eq)]
enum PublishedChecksum {
    /// The release genuinely publishes no digest for this asset. The declared
    /// lengths are then the whole integrity story — a statement, not a gap.
    NotPublished,
    /// The digest the release records for this asset.
    Digest(String),
    /// A checksum manifest exists (or its existence could not be ruled out) and
    /// could not be fetched, read, or used for this asset. Must fail the
    /// install.
    Unavailable(String),
}

/// Resolve what `release` publishes as the digest of `asset_name`.
///
/// With release metadata in hand the asset listing is authoritative: no
/// `SHA256SUMS` asset means no digest, full stop. Without it — the API was
/// rate-limited or erroring — we cannot ask the listing anything, so we ask the
/// download host directly: a 404 there is still a definitive "not published",
/// while anything else is a gap the install must not paper over.
fn published_checksum(release: Option<&Release>, tag: &str, asset_name: &str) -> PublishedChecksum {
    match release {
        Some(release) => match release
            .assets
            .iter()
            .find(|asset| asset.name == CHECKSUMS_ASSET)
        {
            Some(asset) => fetch_published_checksum(&asset.browser_download_url, asset_name, true),
            None => PublishedChecksum::NotPublished,
        },
        None => fetch_published_checksum(
            &direct_release_asset_url(tag, CHECKSUMS_ASSET),
            asset_name,
            false,
        ),
    }
}

/// Fetch and parse a `SHA256SUMS` manifest. `listed` says whether the release
/// metadata asserted that this manifest exists — if it did, a 404 is a
/// contradiction we must report, not an absence we may assume.
fn fetch_published_checksum(url: &str, asset_name: &str, listed: bool) -> PublishedChecksum {
    let allow_loopback = super::test_override_active("OVM_PI_RELEASES_URL");
    if let Err(error) =
        super::validate_download_url(url, super::GITHUB_DOWNLOAD_HOSTS, allow_loopback)
    {
        return PublishedChecksum::Unavailable(format!("rejected {CHECKSUMS_ASSET} URL: {error}"));
    }
    let client = match release_asset_client() {
        Ok(client) => client,
        Err(error) => return PublishedChecksum::Unavailable(format!("no HTTP client: {error}")),
    };
    let mut response = match client.get(url).send() {
        Ok(response) => response,
        Err(error) => {
            return PublishedChecksum::Unavailable(format!("request failed: {error}"));
        }
    };
    if let Err(error) = super::validate_download_url(
        response.url().as_str(),
        super::GITHUB_DOWNLOAD_HOSTS,
        allow_loopback,
    ) {
        return PublishedChecksum::Unavailable(format!("rejected redirect target: {error}"));
    }
    if response.status() == reqwest::StatusCode::NOT_FOUND && !listed {
        // We probed for a manifest the release never claimed to have, and the
        // host says it does not exist. That is real absence.
        return PublishedChecksum::NotPublished;
    }
    if !response.status().is_success() {
        return PublishedChecksum::Unavailable(format!("HTTP {}", response.status()));
    }

    let mut body = String::new();
    // Read one byte past the cap: a manifest larger than the cap must refuse,
    // not be silently truncated to a valid-looking prefix — a conflicting line
    // past the boundary would otherwise never be seen, and an unseen
    // contradiction reads exactly like a clean manifest.
    if let Err(error) = Read::take(&mut response, MAX_CHECKSUMS_BYTES + 1).read_to_string(&mut body)
    {
        return PublishedChecksum::Unavailable(format!("unreadable manifest body: {error}"));
    }
    if body.len() as u64 > MAX_CHECKSUMS_BYTES {
        return PublishedChecksum::Unavailable(format!(
            "manifest exceeds {MAX_CHECKSUMS_BYTES} bytes; refusing to judge a truncated view"
        ));
    }
    match manifest_entry(&body, asset_name) {
        ManifestEntry::Digest(digest) => PublishedChecksum::Digest(digest),
        // A manifest that parsed and simply covers other assets is a real
        // "nothing published for this one".
        ManifestEntry::NotListed => PublishedChecksum::NotPublished,
        // A manifest that names our asset and then cannot speak for it is
        // damaged *for us* — the one thing it had to say, it said wrongly.
        ManifestEntry::Damaged(reason) => PublishedChecksum::Unavailable(reason),
        // A body with no digest line at all is not a manifest we understood
        // (truncated, an error page, a format change), and must not be
        // mistaken for one.
        ManifestEntry::NotAManifest => PublishedChecksum::Unavailable(
            "the manifest contained no sha256 digest lines (truncated, or not a manifest)".into(),
        ),
    }
}

/// What a `SHA256SUMS` body says about one asset.
///
/// Four states, because three cannot hold them. Folding the last two into
/// "nothing published for this asset" is what let a manifest that *names* our
/// tarball with a broken or self-contradicting digest install on length checks
/// alone — the manifest's one relevant line unusable, reported as its absence.
#[derive(Debug, PartialEq, Eq)]
enum ManifestEntry {
    /// Exactly one digest is recorded for this asset. Duplicate lines that
    /// agree collapse into it: they say the same thing twice, which is
    /// redundant, not contradictory.
    Digest(String),
    /// The body is a digest manifest and records no line for this asset.
    NotListed,
    /// The manifest names this asset but cannot be used for it: the digest is
    /// malformed, or two lines disagree about it.
    Damaged(String),
    /// No well-formed digest line anywhere, so this is not a manifest at all.
    NotAManifest,
}

fn is_sha256_hex(token: &str) -> bool {
    token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit())
}

/// Split a `sha256sum`-format line into `(digest-token, asset-name)`, dropping
/// the `*` that binary mode prefixes the name with.
fn split_digest_line(line: &str) -> Option<(&str, &str)> {
    let (digest, name) = line.trim().split_once(char::is_whitespace)?;
    Some((digest, name.trim().trim_start_matches('*')))
}

/// Read a `sha256sum`-format manifest (`<64 hex>  <name>`) for one asset.
fn manifest_entry(manifest: &str, asset_name: &str) -> ManifestEntry {
    let mut any_digest_line = false;
    let mut ours: Option<String> = None;

    for line in manifest.lines() {
        let Some((digest, name)) = split_digest_line(line) else {
            continue;
        };
        let well_formed = is_sha256_hex(digest);
        any_digest_line |= well_formed;
        if name != asset_name {
            continue;
        }
        if !well_formed {
            return ManifestEntry::Damaged(format!(
                "the manifest's line for {asset_name} carries `{}`, which is not a sha256 digest",
                digest_excerpt(digest)
            ));
        }
        let digest = digest.to_ascii_lowercase();
        match &ours {
            Some(first) if *first != digest => {
                return ManifestEntry::Damaged(format!(
                    "the manifest records two different digests for {asset_name} \
                     ({first} and {digest})"
                ));
            }
            Some(_) => {}
            None => ours = Some(digest),
        }
    }

    match ours {
        Some(digest) => ManifestEntry::Digest(digest),
        None if any_digest_line => ManifestEntry::NotListed,
        None => ManifestEntry::NotAManifest,
    }
}

/// A short, escaped excerpt of a token that came off the network, safe to put
/// in an error message (an unparseable body is often HTML, and can be long).
fn digest_excerpt(token: &str) -> String {
    const MAX_CHARS: usize = 16;
    let excerpt: String = token
        .chars()
        .take(MAX_CHARS)
        .flat_map(char::escape_debug)
        .collect();
    if token.chars().count() > MAX_CHARS {
        format!("{excerpt}…")
    } else {
        excerpt
    }
}

fn download_asset(url: &str, dest: &Path, metadata_size: Option<u64>) -> Result<String> {
    // Loopback is only legitimate when the Pi releases test override points at a
    // local mock; production release metadata must never resolve to loopback.
    let allow_loopback = super::test_override_active("OVM_PI_RELEASES_URL");
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

/// Extract the full Pi tarball (bundle with binary, package.json, themes, etc.)
fn extract_full_archive(archive_path: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive_path)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);

    std::fs::create_dir_all(dest)?;

    for entry in archive.entries()? {
        let mut entry = entry.map_err(|e| {
            crate::sources::codex::archive_read_error(e, "unreadable archive entry")
        })?;
        let entry_path = entry
            .path()
            .map_err(|e| {
                crate::sources::codex::archive_read_error(e, "unreadable archive entry path")
            })?
            .into_owned();

        let full_path = super::validate_tar_entry_path(&entry_path, dest)?;

        // Effective (PAX-aware) size, not the raw header size: a PAX `size`
        // extended header overrides the header and drives how many bytes
        // `unpack` streams, so validating the header alone could be bypassed.
        let declared_size = entry.size();
        super::validate_tar_entry_size(declared_size, &entry_path)?;

        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            std::fs::create_dir_all(&full_path)?;
        } else if entry_type.is_file() {
            if let Some(parent) = full_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            entry.unpack(&full_path).map_err(|e| {
                crate::sources::codex::archive_read_error(
                    e,
                    &format!("failed to unpack {}", entry_path.display()),
                )
            })?;
        } else {
            return Err(OvmError::ExtractionFailed(format!(
                "unsupported archive entry type for {}",
                entry_path.display()
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        download_release, extract_full_archive, format_tag, get_latest_npm_release_version_at,
        get_latest_version, list_remote_versions_at, manifest_entry, ManifestEntry,
    };
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use mockito::Server;
    use sha2::Digest;
    use tar::Builder;
    use tempfile::tempdir;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // ── Security: path traversal protection ──────────────────────────

    #[test]
    fn extract_full_archive_rejects_path_traversal() {
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("malicious.tar.gz");
        let dest = dir.path().join("safe_dest");

        create_raw_tar_gz(&archive_path, b"../evil.txt", b"malicious");

        let result = extract_full_archive(&archive_path, &dest);
        assert!(result.is_err(), "path traversal entry must be rejected");

        assert!(
            !dir.path().join("evil.txt").exists(),
            "file must not be written outside destination"
        );
    }

    #[test]
    fn extract_full_archive_rejects_nested_path_traversal() {
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("sneaky.tar.gz");
        let dest = dir.path().join("safe_dest");

        create_raw_tar_gz(&archive_path, b"pi/../../evil.txt", b"sneaky");

        let result = extract_full_archive(&archive_path, &dest);
        assert!(result.is_err(), "nested path traversal must be rejected");
    }

    #[test]
    fn extract_full_archive_rejects_symlink_entries() {
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("symlink.tar.gz");
        let dest = dir.path().join("safe_dest");

        create_raw_tar_gz_entry(&archive_path, b"pi/pi", b"2", b"../../outside", b"");

        let result = extract_full_archive(&archive_path, &dest);
        assert!(result.is_err(), "symlink entries must be rejected");
        assert!(!dest.join("pi/pi").exists());
    }

    #[test]
    fn extract_full_archive_rejects_hardlink_entries() {
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("hardlink.tar.gz");
        let dest = dir.path().join("safe_dest");

        create_raw_tar_gz_entry(&archive_path, b"pi/pi", b"1", b"../../outside", b"");

        let result = extract_full_archive(&archive_path, &dest);
        assert!(result.is_err(), "hardlink entries must be rejected");
        assert!(!dest.join("pi/pi").exists());
    }

    #[test]
    fn extract_full_archive_rejects_special_entries() {
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("special.tar.gz");
        let dest = dir.path().join("safe_dest");

        create_raw_tar_gz_entry(&archive_path, b"pi/device", b"3", b"", b"");

        let result = extract_full_archive(&archive_path, &dest);
        assert!(result.is_err(), "special entries must be rejected");
    }

    #[test]
    fn extract_full_archive_extracts_normal_archive() {
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("good.tar.gz");
        let dest = dir.path().join("output");

        create_safe_tar_gz(
            &archive_path,
            &[("pi/pi", b"fake-binary"), ("pi/package.json", b"{}")],
        );

        extract_full_archive(&archive_path, &dest).expect("normal archive should extract");

        assert!(dest.join("pi").join("pi").exists());
        assert!(dest.join("pi").join("package.json").exists());
    }

    /// Create a tar.gz by writing raw bytes, bypassing the tar crate's
    /// path validation so we can craft genuinely malicious archives.
    fn create_raw_tar_gz(path: &std::path::Path, entry_name: &[u8], contents: &[u8]) {
        create_raw_tar_gz_entry(path, entry_name, b"0", b"", contents);
    }

    fn create_raw_tar_gz_entry(
        path: &std::path::Path,
        entry_name: &[u8],
        entry_type: &[u8; 1],
        link_name: &[u8],
        contents: &[u8],
    ) {
        use std::io::Write;

        let mut header = [0u8; 512];

        let len = entry_name.len().min(99);
        header[..len].copy_from_slice(&entry_name[..len]);

        header[100..108].copy_from_slice(b"0000755\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");

        let size_str = format!("{:011o}\0", contents.len());
        header[124..136].copy_from_slice(size_str.as_bytes());

        header[136..148].copy_from_slice(b"00000000000\0");
        header[156] = entry_type[0];

        let link_len = link_name.len().min(99);
        header[157..157 + link_len].copy_from_slice(&link_name[..link_len]);

        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");

        header[148..156].copy_from_slice(b"        ");
        let cksum: u32 = header.iter().map(|&b| b as u32).sum();
        let cksum_str = format!("{:06o}\0 ", cksum);
        header[148..156].copy_from_slice(&cksum_str.as_bytes()[..8]);

        let mut data_block = vec![0u8; contents.len().div_ceil(512) * 512];
        data_block[..contents.len()].copy_from_slice(contents);

        let end = [0u8; 1024];

        let file = std::fs::File::create(path).expect("create file");
        let mut gz = GzEncoder::new(file, Compression::default());
        gz.write_all(&header).expect("write header");
        gz.write_all(&data_block).expect("write data");
        gz.write_all(&end).expect("write end");
        gz.finish().expect("finish gzip");
    }

    #[test]
    fn extract_full_archive_rejects_oversized_entry() {
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("bomb.tar.gz");
        let dest = dir.path().join("out");

        // A safe-pathed regular file whose header declares ~8 GiB (above the
        // 4 GiB cap) with no data behind it. Extraction must reject on the
        // declared size before reading or writing the entry.
        create_tar_gz_with_declared_size(&archive_path, b"pi/huge.bin", 0o77777777777);

        let error =
            extract_full_archive(&archive_path, &dest).expect_err("oversized entry rejected");
        assert!(error.to_string().contains("oversized"), "{error}");
        assert!(!dest.join("pi/huge.bin").exists());
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

    /// Create a well-formed tar.gz using the builder (safe paths only).
    fn create_safe_tar_gz(path: &std::path::Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).expect("create archive");
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);

        for (name, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, name, &contents[..])
                .expect("append entry");
        }

        let encoder = builder.into_inner().expect("finish tar");
        encoder.finish().expect("finish gzip");
    }

    #[test]
    fn format_tag_adds_v_prefix() {
        assert_eq!(format_tag("0.67.6"), "v0.67.6");
        assert_eq!(format_tag("v0.67.6"), "v0.67.6");
        assert_eq!(format_tag("latest"), "latest");
    }

    #[test]
    fn list_remote_versions_collects_across_pages() {
        let mut server = Server::new();
        let _p1 = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::UrlEncoded("page".into(), "1".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"[{"tag_name":"v0.67.6","assets":[]},{"tag_name":"v0.67.5","assets":[]}]"#)
            .create();
        let _p2 = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::UrlEncoded("page".into(), "2".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .create();

        let versions = list_remote_versions_at(&server.url()).expect("success");
        assert_eq!(versions, vec!["0.67.6", "0.67.5"]);
    }

    #[test]
    fn get_latest_version_normalizes_v_prefix() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/latest")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"tag_name":"v0.74.0","assets":[]}"#)
            .create();

        std::env::set_var("OVM_PI_RELEASES_URL", server.url());
        std::env::set_var("OVM_PI_NPM_REGISTRY_URL", server.url());
        let latest = get_latest_version().expect("success");
        std::env::remove_var("OVM_PI_RELEASES_URL");
        std::env::remove_var("OVM_PI_NPM_REGISTRY_URL");

        assert_eq!(latest, "0.74.0");
    }

    #[test]
    fn npm_latest_dist_tag_returns_release_version() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"dist-tags":{"latest":"0.79.10","alpha":"0.80.0-alpha.1"}}"#)
            .create();

        let latest = get_latest_npm_release_version_at(&server.url()).expect("latest");

        assert_eq!(latest, "0.79.10");
    }

    #[test]
    fn npm_latest_dist_tag_rejects_prerelease() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"dist-tags":{"latest":"0.80.0-alpha.1"}}"#)
            .create();

        assert!(get_latest_npm_release_version_at(&server.url()).is_err());
    }

    #[test]
    fn download_release_falls_back_to_direct_release_asset_url() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let mut server = Server::new();
        let asset_name = super::expected_asset_name();
        let dir = tempdir().expect("tempdir");
        let archive_path = dir.path().join("pi.tar.gz");
        create_safe_tar_gz(
            &archive_path,
            &[("pi/pi", b"fake-binary"), ("pi/package.json", b"{}")],
        );
        let asset_body = std::fs::read(&archive_path).expect("read archive");

        let _api = server
            .mock("GET", "/tags/v0.79.10")
            .with_status(403)
            .create();
        let _asset = server
            .mock("GET", format!("/download/v0.79.10/{asset_name}").as_str())
            .with_status(200)
            .with_header("content-type", "application/octet-stream")
            .with_body(asset_body)
            .create();
        // With the API unreadable we probe the download host for a manifest.
        // 404 is the host stating this release publishes none.
        let _sums = server
            .mock("GET", "/download/v0.79.10/SHA256SUMS")
            .with_status(404)
            .create();

        std::env::set_var("OVM_PI_RELEASES_URL", server.url());
        let bundle_dir = dir.path().join("bundle");
        let metadata = download_release("0.79.10", &bundle_dir).expect("download");
        std::env::remove_var("OVM_PI_RELEASES_URL");

        assert!(bundle_dir.join("pi/pi").exists());
        assert_eq!(metadata.resolved_tag, "v0.79.10");
        assert_eq!(metadata.asset_name, asset_name);
    }

    // ── Download integrity ───────────────────────────────────────────

    /// The line between "a manifest that does not cover our asset" (real
    /// absence) and "a body that is not a manifest at all" (a gap we must
    /// report). Only the first may install on length checks.
    #[test]
    fn a_manifest_is_recognized_only_by_a_well_formed_digest_line() {
        let ours = "pi-darwin-arm64.tar.gz";
        assert_eq!(
            manifest_entry(
                &format!("{}  pi-0.83.0-source.tar.gz\n", "a".repeat(64)),
                ours
            ),
            ManifestEntry::NotListed
        );
        assert_eq!(manifest_entry("", ours), ManifestEntry::NotAManifest);
        assert_eq!(
            manifest_entry("<html>503 Service Unavailable</html>", ours),
            ManifestEntry::NotAManifest
        );
        // Truncated: the digest is cut short, so this is not a digest line —
        // and since it is *our* line, the manifest is damaged for us.
        assert!(matches!(
            manifest_entry(&format!("{}  {ours}", "a".repeat(40)), ours),
            ManifestEntry::Damaged(_)
        ));
    }

    #[test]
    fn sha256sums_lookup_matches_only_the_named_asset() {
        let manifest = "\
f225b87ec3b4825dd5b94e922a8629558addca31a1b4d2c206ae598a8e2692c0  pi-0.83.0-source.tar.gz
147FC3C451EC543A15102AF251CE316079C8FCADFE8AE4D3FFEE202346E9BED9  pi-darwin-arm64.tar.gz
b0625eb623197b0afe20c870d21ef2f34481f1504e5777df3f698a66c7636f5f *pi-linux-x64.tar.gz
not-a-digest  pi-linux-arm64.tar.gz
";
        assert_eq!(
            manifest_entry(manifest, "pi-darwin-arm64.tar.gz"),
            ManifestEntry::Digest(
                "147fc3c451ec543a15102af251ce316079c8fcadfe8ae4d3ffee202346e9bed9".into()
            )
        );
        // `*name` (binary mode) is the same asset.
        assert!(matches!(
            manifest_entry(manifest, "pi-linux-x64.tar.gz"),
            ManifestEntry::Digest(_)
        ));
        // No line for the asset means "nothing published to check against".
        assert_eq!(
            manifest_entry(manifest, "pi-windows-x64.zip"),
            ManifestEntry::NotListed
        );
        assert_eq!(
            manifest_entry("", "pi-darwin-arm64.tar.gz"),
            ManifestEntry::NotAManifest
        );
    }

    /// A manifest that names OUR asset and then carries something that is not
    /// a digest is damaged for us, not silent about us. Reading it as absence
    /// (the old `None`) let the install proceed on declared lengths alone —
    /// with the publisher's own manifest sitting right there, unusable.
    #[test]
    fn a_malformed_digest_line_for_our_asset_is_damage_not_absence() {
        let ours = "pi-darwin-arm64.tar.gz";
        // The manifest parses fine — another asset's line is well-formed — so
        // "not a manifest" cannot be the answer either.
        let manifest = format!(
            "{}  pi-linux-x64.tar.gz\nnot-a-digest  {ours}\n",
            "a".repeat(64)
        );

        let entry = manifest_entry(&manifest, ours);

        let ManifestEntry::Damaged(reason) = entry else {
            panic!("a malformed line for our own asset must be damage: {entry:?}");
        };
        assert!(reason.contains(ours), "{reason}");
        assert!(reason.contains("not a sha256 digest"), "{reason}");
    }

    /// Two well-formed lines that disagree make the manifest unusable for this
    /// asset: first-match-wins would accept an archive matching whichever line
    /// happened to come first, so a single appended line could pick the digest.
    /// Duplicates that agree are redundant, not contradictory, and still pass.
    #[test]
    fn conflicting_duplicate_lines_make_the_manifest_unusable() {
        let ours = "pi-darwin-arm64.tar.gz";
        let first = "a".repeat(64);
        let second = "b".repeat(64);

        let entry = manifest_entry(&format!("{first}  {ours}\n{second}  {ours}\n"), ours);
        let ManifestEntry::Damaged(reason) = entry else {
            panic!("conflicting duplicates must not resolve to a digest: {entry:?}");
        };
        assert!(reason.contains("two different digests"), "{reason}");
        assert!(reason.contains(ours), "{reason}");

        // Identical duplicates (including a case difference, since digests are
        // compared case-insensitively) say the same thing twice.
        assert_eq!(
            manifest_entry(
                &format!("{first}  {ours}\n{}  *{ours}\n", first.to_ascii_uppercase()),
                ours
            ),
            ManifestEntry::Digest(first)
        );
    }

    /// Serve a release whose SHA256SUMS lists `digest` for the platform asset.
    fn release_with_checksums(
        server: &mut Server,
        digest: &str,
        body: Vec<u8>,
    ) -> Vec<mockito::Mock> {
        let manifest = format!("{digest}  {}\n", super::expected_asset_name());
        release_with_checksum_body(server, &manifest, body)
    }

    /// Serve a release whose SHA256SUMS asset has an arbitrary body.
    fn release_with_checksum_body(
        server: &mut Server,
        manifest: &str,
        body: Vec<u8>,
    ) -> Vec<mockito::Mock> {
        let asset_name = super::expected_asset_name();
        let base = server.url();
        let release_json = format!(
            r#"{{"tag_name":"v0.83.0","assets":[
                {{"name":"{asset_name}","browser_download_url":"{base}/download/v0.83.0/{asset_name}","size":{}}},
                {{"name":"SHA256SUMS","browser_download_url":"{base}/download/v0.83.0/SHA256SUMS"}}
            ]}}"#,
            body.len()
        );
        vec![
            server
                .mock("GET", "/tags/v0.83.0")
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(release_json)
                .create(),
            server
                .mock("GET", format!("/download/v0.83.0/{asset_name}").as_str())
                .with_status(200)
                .with_header("content-type", "application/octet-stream")
                .with_body(body)
                .create(),
            server
                .mock("GET", "/download/v0.83.0/SHA256SUMS")
                .with_status(200)
                .with_header("content-type", "text/plain")
                .with_body(manifest)
                .create(),
        ]
    }

    fn pi_archive(dir: &std::path::Path) -> Vec<u8> {
        let archive_path = dir.join("pi.tar.gz");
        create_safe_tar_gz(
            &archive_path,
            &[("pi/pi", b"fake-binary"), ("pi/package.json", b"{}")],
        );
        std::fs::read(&archive_path).expect("read archive")
    }

    /// Pi publishes SHA256SUMS with its releases; an archive that does not
    /// match it must never be unpacked and installed.
    #[test]
    fn a_checksum_mismatch_against_published_sha256sums_fails_the_install() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let dir = tempdir().expect("tempdir");
        let mut server = Server::new();
        // A perfectly well-formed archive: only the published digest
        // disagrees, so nothing but the checksum check can stop the install.
        let _mocks = release_with_checksums(&mut server, &"0".repeat(64), pi_archive(dir.path()));

        std::env::set_var("OVM_PI_RELEASES_URL", server.url());
        let bundle_dir = dir.path().join("bundle");
        let result = download_release("0.83.0", &bundle_dir);
        std::env::remove_var("OVM_PI_RELEASES_URL");

        let error = result.expect_err("a mismatched archive must not install");
        let message = error.to_string();
        assert!(message.contains("SHA256SUMS"), "{message}");
        assert!(message.contains("checksum mismatch"), "{message}");
        assert!(
            !bundle_dir.join("pi/pi").exists(),
            "nothing may be unpacked from an unverified archive"
        );
        assert!(!bundle_dir.join("pi.tar.gz").exists());
    }

    /// The matching case still installs — the check must not be a blanket fail.
    #[test]
    fn a_matching_published_checksum_installs() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let dir = tempdir().expect("tempdir");
        let body = pi_archive(dir.path());
        let digest: String = sha2::Sha256::digest(&body)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let mut server = Server::new();
        let _mocks = release_with_checksums(&mut server, &digest, body);

        std::env::set_var("OVM_PI_RELEASES_URL", server.url());
        let bundle_dir = dir.path().join("bundle");
        let metadata = download_release("0.83.0", &bundle_dir);
        std::env::remove_var("OVM_PI_RELEASES_URL");

        let metadata = metadata.expect("verified archive installs");
        assert_eq!(metadata.resolved_tag, "v0.83.0");
        assert!(bundle_dir.join("pi/pi").exists());
    }

    /// The distinction the whole finding is about: a release that publishes a
    /// SHA256SUMS we cannot fetch must FAIL, not fall back to length checks.
    /// A same-length substituted archive would otherwise install cleanly.
    #[test]
    fn an_unfetchable_published_checksum_fails_the_install() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempdir().expect("tempdir");
        let mut server = Server::new();
        let asset_name = super::expected_asset_name();
        let body = pi_archive(dir.path());
        let base = server.url();
        let release_json = format!(
            r#"{{"tag_name":"v0.83.0","assets":[
                {{"name":"{asset_name}","browser_download_url":"{base}/download/v0.83.0/{asset_name}","size":{}}},
                {{"name":"SHA256SUMS","browser_download_url":"{base}/download/v0.83.0/SHA256SUMS"}}
            ]}}"#,
            body.len()
        );
        let _api = server
            .mock("GET", "/tags/v0.83.0")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(release_json)
            .create();
        let _asset = server
            .mock("GET", format!("/download/v0.83.0/{asset_name}").as_str())
            .with_status(200)
            .with_body(body)
            .create();
        // The manifest is published — the release listing says so — but the
        // host cannot serve it right now.
        let _sums = server
            .mock("GET", "/download/v0.83.0/SHA256SUMS")
            .with_status(500)
            .create();

        std::env::set_var("OVM_PI_RELEASES_URL", server.url());
        let bundle_dir = dir.path().join("bundle");
        let result = download_release("0.83.0", &bundle_dir);
        std::env::remove_var("OVM_PI_RELEASES_URL");

        let error = result.expect_err("an unverifiable checksum must not install");
        let message = error.to_string();
        assert!(message.contains("could not be verified"), "{message}");
        assert!(message.contains("length checks alone"), "{message}");
        assert!(!bundle_dir.join("pi/pi").exists());
    }

    /// Same shape, one layer down: the manifest is served but its body is not a
    /// digest manifest (an error page, a truncated transfer). "Unparseable" is
    /// not "absent".
    #[test]
    fn a_malformed_published_checksum_fails_the_install() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempdir().expect("tempdir");
        let mut server = Server::new();
        let _mocks = release_with_checksum_body(
            &mut server,
            "<html><body>503 Service Unavailable</body></html>",
            pi_archive(dir.path()),
        );

        std::env::set_var("OVM_PI_RELEASES_URL", server.url());
        let bundle_dir = dir.path().join("bundle");
        let result = download_release("0.83.0", &bundle_dir);
        std::env::remove_var("OVM_PI_RELEASES_URL");

        let error = result.expect_err("an unparseable manifest must not install");
        assert!(
            error.to_string().contains("no sha256 digest lines"),
            "{error}"
        );
        assert!(!bundle_dir.join("pi/pi").exists());
    }

    /// The other side of the distinction: a manifest that parsed fine and
    /// simply does not cover our platform asset really is "nothing published
    /// for this one", and must still install.
    #[test]
    fn a_manifest_without_our_asset_installs_on_length_checks() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempdir().expect("tempdir");
        let mut server = Server::new();
        let _mocks = release_with_checksum_body(
            &mut server,
            &format!("{}  pi-0.83.0-source.tar.gz\n", "a".repeat(64)),
            pi_archive(dir.path()),
        );

        std::env::set_var("OVM_PI_RELEASES_URL", server.url());
        let bundle_dir = dir.path().join("bundle");
        let metadata = download_release("0.83.0", &bundle_dir);
        std::env::remove_var("OVM_PI_RELEASES_URL");

        metadata.expect("an asset with no published digest still installs");
        assert!(bundle_dir.join("pi/pi").exists());
    }

    /// End to end: a manifest that names our asset with a broken digest must
    /// stop the install. It used to read as "this release publishes no digest
    /// for us" and install on declared lengths alone.
    #[test]
    fn a_manifest_naming_our_asset_with_a_bad_digest_fails_the_install() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempdir().expect("tempdir");
        let mut server = Server::new();
        // Well-formed for another asset, broken for ours: neither "not a
        // manifest" nor "nothing published for us" describes this.
        let manifest = format!(
            "{}  pi-0.83.0-source.tar.gz\nnot-a-digest  {}\n",
            "a".repeat(64),
            super::expected_asset_name()
        );
        let _mocks = release_with_checksum_body(&mut server, &manifest, pi_archive(dir.path()));

        std::env::set_var("OVM_PI_RELEASES_URL", server.url());
        let bundle_dir = dir.path().join("bundle");
        let result = download_release("0.83.0", &bundle_dir);
        std::env::remove_var("OVM_PI_RELEASES_URL");

        let error = result.expect_err("a damaged digest line must not install");
        let message = error.to_string();
        assert!(message.contains("not a sha256 digest"), "{message}");
        assert!(message.contains("length checks alone"), "{message}");
        assert!(!bundle_dir.join("pi/pi").exists());
    }

    /// A manifest larger than the read cap must refuse, not be judged by a
    /// valid-looking prefix: a conflicting line past the boundary would never
    /// be seen, and an unseen contradiction reads exactly like a clean
    /// manifest. The body here carries a correct digest for our asset inside
    /// the first mebibyte — precisely the case a silent truncation would
    /// happily install.
    #[test]
    fn a_manifest_larger_than_the_cap_is_refused_not_truncated() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempdir().expect("tempdir");
        let body = pi_archive(dir.path());
        let real: String = sha2::Sha256::digest(&body)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let mut manifest = format!("{real}  {}\n", super::expected_asset_name());
        let filler = format!("{}  some-other-asset.tar.gz\n", "b".repeat(64));
        while (manifest.len() as u64) <= super::MAX_CHECKSUMS_BYTES {
            manifest.push_str(&filler);
        }
        let mut server = Server::new();
        let _mocks = release_with_checksum_body(&mut server, &manifest, body);
        std::env::set_var("OVM_PI_RELEASES_URL", server.url());
        let bundle_dir = dir.path().join("bundle");
        let result = download_release("0.83.0", &bundle_dir);
        std::env::remove_var("OVM_PI_RELEASES_URL");

        let error = result.expect_err("an oversized manifest must refuse, not truncate");
        let message = error.to_string();
        assert!(
            message.contains("refusing to judge a truncated view"),
            "{message}"
        );
        assert!(!bundle_dir.join("pi/pi").exists());
    }

    /// Same shape for conflicting duplicates: two valid lines that disagree
    /// leave no digest we may act on, so the install must refuse rather than
    /// take whichever came first.
    #[test]
    fn conflicting_duplicate_digest_lines_fail_the_install() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempdir().expect("tempdir");
        let body = pi_archive(dir.path());
        // The FIRST line is the archive's real digest, so a first-match-wins
        // parser would install happily.
        let real: String = sha2::Sha256::digest(&body)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let asset = super::expected_asset_name();
        let manifest = format!("{real}  {asset}\n{}  {asset}\n", "b".repeat(64));
        let mut server = Server::new();
        let _mocks = release_with_checksum_body(&mut server, &manifest, body);

        std::env::set_var("OVM_PI_RELEASES_URL", server.url());
        let bundle_dir = dir.path().join("bundle");
        let result = download_release("0.83.0", &bundle_dir);
        std::env::remove_var("OVM_PI_RELEASES_URL");

        let error = result.expect_err("a self-contradicting manifest must not install");
        let message = error.to_string();
        assert!(message.contains("two different digests"), "{message}");
        assert!(!bundle_dir.join("pi/pi").exists());
    }

    /// GitHub always declares an asset's size. A release entry that omits one
    /// is metadata we do not understand — installing on `Content-Length` alone
    /// would hide that behind a normal-looking success.
    #[test]
    fn an_asset_without_a_declared_size_fails_the_install() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempdir().expect("tempdir");
        let mut server = Server::new();
        let asset_name = super::expected_asset_name();
        let base = server.url();
        let release_json = format!(
            r#"{{"tag_name":"v0.83.0","assets":[
                {{"name":"{asset_name}","browser_download_url":"{base}/download/v0.83.0/{asset_name}"}}
            ]}}"#
        );
        let _api = server
            .mock("GET", "/tags/v0.83.0")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(release_json)
            .create();
        let _asset = server
            .mock("GET", format!("/download/v0.83.0/{asset_name}").as_str())
            .with_status(200)
            .with_body(pi_archive(dir.path()))
            .create();

        std::env::set_var("OVM_PI_RELEASES_URL", server.url());
        let bundle_dir = dir.path().join("bundle");
        let result = download_release("0.83.0", &bundle_dir);
        std::env::remove_var("OVM_PI_RELEASES_URL");

        let error = result.expect_err("an undeclared asset size must not install");
        assert!(error.to_string().contains("declares no size"), "{error}");
    }

    #[test]
    fn list_remote_versions_errors_on_5xx() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(500)
            .create();
        let result = list_remote_versions_at(&server.url());
        assert!(result.is_err());
    }
}
