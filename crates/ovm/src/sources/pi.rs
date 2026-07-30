use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::release_metadata::ReleaseInstallMetadata;
use crate::sources::codex::Release;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{self, Read};
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
    let (resolved_tag, asset_name, download_url) = if let Ok(release) = fetch_release(&tag) {
        if let Some(asset) = release
            .assets
            .iter()
            .find(|asset| asset.name == expected_asset_name())
        {
            (
                release.tag_name,
                asset.name.clone(),
                asset.browser_download_url.clone(),
            )
        } else {
            direct_release_asset(&tag)
        }
    } else {
        direct_release_asset(&tag)
    };

    std::fs::create_dir_all(bundle_dir)?;

    let archive_path = bundle_dir.join("pi.tar.gz");
    let archive_sha256 = download_asset(&download_url, &archive_path)?;
    extract_full_archive(&archive_path, bundle_dir)?;
    let _ = std::fs::remove_file(&archive_path);

    // Verify the binary exists
    let binary = bundle_dir.join("pi").join("pi");
    if !binary.exists() {
        return Err(OvmError::ExtractionFailed(
            "Pi binary not found in extracted bundle".into(),
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

fn direct_release_download_url(tag: &str) -> String {
    if releases_api_base() == DEFAULT_RELEASES_API_BASE {
        format!(
            "https://github.com/earendil-works/pi/releases/download/{tag}/{}",
            expected_asset_name()
        )
    } else {
        format!(
            "{}/download/{tag}/{}",
            releases_api_base().trim_end_matches('/'),
            expected_asset_name()
        )
    }
}

fn direct_release_asset(tag: &str) -> (String, String, String) {
    (
        tag.to_string(),
        expected_asset_name().to_string(),
        direct_release_download_url(tag),
    )
}

fn download_asset(url: &str, dest: &Path) -> Result<String> {
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

    let mut file = std::fs::File::create(dest)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];

    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|error| OvmError::DownloadFailed {
                url: url.to_string(),
                message: format!("failed to read release asset body: {error}"),
            })?;
        if read == 0 {
            break;
        }

        io::Write::write_all(&mut file, &buffer[..read]).map_err(|error| {
            OvmError::DownloadFailed {
                url: url.to_string(),
                message: format!(
                    "failed to write release asset to {}: {error}",
                    dest.display()
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

/// Extract the full Pi tarball (bundle with binary, package.json, themes, etc.)
fn extract_full_archive(archive_path: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive_path)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);

    std::fs::create_dir_all(dest)?;

    for entry in archive.entries()? {
        let mut entry = entry.map_err(|e| OvmError::ExtractionFailed(e.to_string()))?;
        let entry_path = entry
            .path()
            .map_err(|e| OvmError::ExtractionFailed(e.to_string()))?
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
            entry
                .unpack(&full_path)
                .map_err(|e| OvmError::ExtractionFailed(e.to_string()))?;
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
        get_latest_version, list_remote_versions_at,
    };
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use mockito::Server;
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

        std::env::set_var("OVM_PI_RELEASES_URL", server.url());
        let bundle_dir = dir.path().join("bundle");
        let metadata = download_release("0.79.10", &bundle_dir).expect("download");
        std::env::remove_var("OVM_PI_RELEASES_URL");

        assert!(bundle_dir.join("pi/pi").exists());
        assert_eq!(metadata.resolved_tag, "v0.79.10");
        assert_eq!(metadata.asset_name, asset_name);
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
