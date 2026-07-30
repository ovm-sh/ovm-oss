use crate::error::{OvmError, Result};
use base64::Engine as _;
use indicatif::{ProgressBar, ProgressStyle};
use serde::Deserialize;
use sha2::{Digest, Sha512};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

const DEFAULT_REGISTRY_URL: &str = "https://registry.npmjs.org/@anthropic-ai/claude-code";

/// Resolve the npm package URL. Tests set `OVM_NPM_PACKAGE_URL` to point at a mock server.
fn registry_url() -> String {
    std::env::var("OVM_NPM_PACKAGE_URL").unwrap_or_else(|_| DEFAULT_REGISTRY_URL.to_string())
}

#[derive(Debug, Deserialize)]
struct PackageInfo {
    versions: HashMap<String, VersionInfo>,
    #[serde(rename = "dist-tags")]
    dist_tags: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct VersionInfo {
    dist: DistInfo,
}

#[derive(Debug, Deserialize)]
struct DistInfo {
    tarball: String,
    /// Subresource-integrity string published by the registry, e.g.
    /// `sha512-<base64>`. Used to verify the downloaded tarball.
    #[serde(default)]
    integrity: Option<String>,
}

/// A resolved tarball download target plus its integrity metadata.
struct TarballRef {
    url: String,
    integrity: Option<String>,
}

fn registry_client(timeout_secs: u64) -> Result<reqwest::blocking::Client> {
    super::http_client(timeout_secs)
}

fn tarball_client(timeout_secs: u64, allowed_hosts: &[&str]) -> Result<reqwest::blocking::Client> {
    super::download_http_client(timeout_secs, allowed_hosts)
}

pub fn list_remote_versions() -> Result<Vec<semver::Version>> {
    list_remote_versions_at(&registry_url())
}

fn list_remote_versions_at(url: &str) -> Result<Vec<semver::Version>> {
    let resp = registry_client(30)?
        .get(url)
        .header("Accept", "application/json")
        .send()?;

    if !resp.status().is_success() {
        return Err(OvmError::DownloadFailed {
            url: url.to_string(),
            message: format!("HTTP {}", resp.status()),
        });
    }

    let info: PackageInfo = resp.json()?;

    let mut versions: Vec<semver::Version> = info
        .versions
        .keys()
        .filter_map(|v| semver::Version::parse(v).ok())
        .collect();

    versions.sort();
    Ok(versions)
}

pub fn get_latest_version() -> Result<String> {
    get_latest_version_at(&registry_url())
}

fn get_latest_version_at(url: &str) -> Result<String> {
    let resp = registry_client(15)?
        .get(url)
        .header("Accept", "application/json")
        .send()?;

    let info: PackageInfo = resp.json()?;

    info.dist_tags
        .get("latest")
        .cloned()
        .ok_or_else(|| OvmError::VersionNotFound("latest".into()))
}

fn get_tarball_ref(version: &str) -> Result<TarballRef> {
    let url = format!("{}/{}", registry_url(), version);
    let resp = registry_client(15)?
        .get(&url)
        .header("Accept", "application/json")
        .send()?;

    if !resp.status().is_success() {
        return Err(OvmError::VersionNotFound(version.to_string()));
    }

    let info: VersionInfo = resp.json()?;
    Ok(TarballRef {
        url: info.dist.tarball,
        integrity: info.dist.integrity,
    })
}

/// Host of the configured registry. The tarball must come from this same host
/// (in addition to the generic HTTPS/loopback rules in `validate_download_url`).
fn registry_host() -> String {
    reqwest::Url::parse(&registry_url())
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default()
}

/// Verify a downloaded tarball against the registry's SHA-512 SRI metadata.
///
/// The `integrity` field may carry several space-separated SRI hashes; we
/// verify the SHA-512 entry, which npm always publishes for modern packages.
/// If no SHA-512 entry is present we cannot verify and proceed (older
/// packages); any other digest mismatch is a hard failure.
pub(crate) fn verify_sha512_integrity(integrity: &str, digest: &[u8]) -> Result<()> {
    let Some(sri) = integrity
        .split_whitespace()
        .find(|hash| hash.starts_with("sha512-"))
    else {
        return Ok(());
    };

    let encoded = &sri["sha512-".len()..];
    let expected = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| OvmError::DownloadFailed {
            url: "npm integrity".to_string(),
            message: format!("invalid integrity metadata from registry: {e}"),
        })?;

    if digest == expected.as_slice() {
        Ok(())
    } else {
        Err(OvmError::DownloadFailed {
            url: "npm integrity".to_string(),
            message: "downloaded tarball SHA-512 does not match registry integrity metadata"
                .to_string(),
        })
    }
}

pub fn download_tarball(version: &str, dest: &Path) -> Result<()> {
    let tarball = get_tarball_ref(version)?;
    let tarball_url = tarball.url;
    let allowed = registry_host();
    // Loopback is only a legitimate tarball host when the test registry override
    // is set; production metadata must never point the download at loopback.
    let allow_loopback = super::test_override_active("OVM_NPM_PACKAGE_URL");

    super::validate_download_url(&tarball_url, &[allowed.as_str()], allow_loopback)?;

    let resp = tarball_client(120, &[allowed.as_str()])?
        .get(&tarball_url)
        .send()?;
    super::validate_download_url(resp.url().as_str(), &[allowed.as_str()], allow_loopback)?;

    if !resp.status().is_success() {
        return Err(OvmError::DownloadFailed {
            url: tarball_url,
            message: format!("HTTP {}", resp.status()),
        });
    }

    let total_size = resp.content_length().unwrap_or(0);

    let pb = ProgressBar::new(total_size);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  {bar:40.cyan/dim} {bytes}/{total_bytes} {msg}")
            .unwrap()
            .progress_chars("██░"),
    );
    pb.set_message(format!("v{}", version));

    crate::util::ensure_parent_dir(dest)?;

    let mut reader = resp;
    let result = stream_tarball_to_file(
        &mut reader,
        dest,
        tarball.integrity.as_deref(),
        &tarball_url,
        &pb,
    );
    pb.finish_and_clear();
    result
}

/// Stream a tarball to `dest`, hashing as it goes, and verify it against the
/// registry's SHA-512 SRI metadata when present.
///
/// On any failure — a mid-stream connection drop (the server promised more via
/// Content-Length then closed) as much as an integrity mismatch — the
/// half-written tarball is removed so it can never be mistaken for a good
/// download.
fn stream_tarball_to_file(
    reader: &mut impl Read,
    dest: &Path,
    integrity: Option<&str>,
    url: &str,
    pb: &ProgressBar,
) -> Result<()> {
    let result = (|| -> Result<()> {
        let mut file = std::fs::File::create(dest)?;
        let mut downloaded: u64 = 0;
        let mut buffer = [0u8; 8192];
        let mut hasher = Sha512::new();

        loop {
            let bytes_read = reader
                .read(&mut buffer)
                .map_err(|e| OvmError::DownloadFailed {
                    url: url.to_string(),
                    message: e.to_string(),
                })?;

            if bytes_read == 0 {
                break;
            }

            std::io::Write::write_all(&mut file, &buffer[..bytes_read])?;
            hasher.update(&buffer[..bytes_read]);
            downloaded += bytes_read as u64;
            pb.set_position(downloaded);
        }

        if let Some(integrity) = integrity {
            verify_sha512_integrity(integrity, &hasher.finalize())?;
        }
        Ok(())
    })();

    if let Err(error) = result {
        let _ = std::fs::remove_file(dest);
        return Err(error);
    }
    Ok(())
}

pub fn extract_tarball(tarball_path: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(tarball_path)?;
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

pub fn npm_install(package_ref: &Path, install_dir: &Path) -> Result<()> {
    let npm = crate::node::find_npm().ok_or(OvmError::NpmNotFound)?;

    std::fs::create_dir_all(install_dir)?;

    let pkg_json = serde_json::json!({
        "name": "ovm-version",
        "private": true,
        "dependencies": {}
    });
    std::fs::write(
        install_dir.join("package.json"),
        serde_json::to_string_pretty(&pkg_json)?,
    )?;

    let status = std::process::Command::new(&npm)
        .args([
            "install",
            "--ignore-scripts",
            &package_ref.display().to_string(),
        ])
        .current_dir(install_dir)
        .env_remove("CLAUDECODE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| OvmError::NpmInstallFailed {
            version: package_ref.display().to_string(),
            message: e.to_string(),
        })?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        return Err(OvmError::NpmInstallFailed {
            version: package_ref.display().to_string(),
            message: format!(
                "exit code {}: {}",
                status.status.code().unwrap_or(-1),
                stderr
            ),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    // ── Security: path traversal protection ──────────────────────────

    #[test]
    fn extract_tarball_rejects_path_traversal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive_path = dir.path().join("malicious.tar.gz");
        let dest = dir.path().join("safe_dest");

        create_raw_tar_gz(&archive_path, b"../evil.txt", b"malicious content");

        let result = extract_tarball(&archive_path, &dest);
        assert!(result.is_err(), "path traversal entry must be rejected");

        assert!(
            !dir.path().join("evil.txt").exists(),
            "file must not be written outside destination"
        );
    }

    #[test]
    fn extract_tarball_rejects_nested_path_traversal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive_path = dir.path().join("sneaky.tar.gz");
        let dest = dir.path().join("safe_dest");

        create_raw_tar_gz(&archive_path, b"package/../../evil.txt", b"sneaky content");

        let result = extract_tarball(&archive_path, &dest);
        assert!(result.is_err(), "nested path traversal must be rejected");
    }

    #[test]
    fn extract_tarball_rejects_symlink_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive_path = dir.path().join("symlink.tar.gz");
        let dest = dir.path().join("safe_dest");

        create_raw_tar_gz_entry(
            &archive_path,
            b"package/bin/claude",
            b"2",
            b"../../outside",
            b"",
        );

        let result = extract_tarball(&archive_path, &dest);
        assert!(result.is_err(), "symlink entries must be rejected");
        assert!(!dest.join("package/bin/claude").exists());
    }

    #[test]
    fn extract_tarball_rejects_hardlink_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive_path = dir.path().join("hardlink.tar.gz");
        let dest = dir.path().join("safe_dest");

        create_raw_tar_gz_entry(
            &archive_path,
            b"package/bin/claude",
            b"1",
            b"../../outside",
            b"",
        );

        let result = extract_tarball(&archive_path, &dest);
        assert!(result.is_err(), "hardlink entries must be rejected");
        assert!(!dest.join("package/bin/claude").exists());
    }

    #[test]
    fn extract_tarball_rejects_special_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive_path = dir.path().join("special.tar.gz");
        let dest = dir.path().join("safe_dest");

        create_raw_tar_gz_entry(&archive_path, b"package/device", b"3", b"", b"");

        let result = extract_tarball(&archive_path, &dest);
        assert!(result.is_err(), "special entries must be rejected");
    }

    #[test]
    fn extract_tarball_extracts_normal_archive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive_path = dir.path().join("good.tar.gz");
        let dest = dir.path().join("output");

        create_safe_tar_gz(
            &archive_path,
            &[
                ("package/index.js", b"console.log('hello');"),
                ("package/package.json", b"{}"),
            ],
        );

        extract_tarball(&archive_path, &dest).expect("normal archive should extract");

        assert!(dest.join("package").join("index.js").exists());
        assert!(dest.join("package").join("package.json").exists());
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

        // Name (bytes 0-99)
        let len = entry_name.len().min(99);
        header[..len].copy_from_slice(&entry_name[..len]);

        // Mode, UID, GID
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");

        // Size (octal)
        let size_str = format!("{:011o}\0", contents.len());
        header[124..136].copy_from_slice(size_str.as_bytes());

        // Mtime
        header[136..148].copy_from_slice(b"00000000000\0");

        header[156] = entry_type[0];

        let link_len = link_name.len().min(99);
        header[157..157 + link_len].copy_from_slice(&link_name[..link_len]);

        // Magic + version
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");

        // Checksum: fill with spaces, compute, write
        header[148..156].copy_from_slice(b"        ");
        let cksum: u32 = header.iter().map(|&b| b as u32).sum();
        let cksum_str = format!("{:06o}\0 ", cksum);
        header[148..156].copy_from_slice(&cksum_str.as_bytes()[..8]);

        // Data padded to 512
        let mut data_block = vec![0u8; contents.len().div_ceil(512) * 512];
        data_block[..contents.len()].copy_from_slice(contents);

        let end = [0u8; 1024];

        let file = std::fs::File::create(path).expect("create file");
        let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        gz.write_all(&header).expect("write header");
        gz.write_all(&data_block).expect("write data");
        gz.write_all(&end).expect("write end");
        gz.finish().expect("finish gzip");
    }

    /// Create a well-formed tar.gz using the builder (safe paths only).
    fn create_safe_tar_gz(path: &std::path::Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).expect("create archive");
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);

        for (name, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, &contents[..])
                .expect("append entry");
        }

        let encoder = builder.into_inner().expect("finish tar");
        encoder.finish().expect("finish gzip");
    }

    // ── Security: npm integrity verification ─────────────────────────

    fn sha512_sri(data: &[u8]) -> String {
        let digest = Sha512::digest(data);
        format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(digest)
        )
    }

    #[test]
    fn verify_sha512_integrity_accepts_matching_digest() {
        let data = b"the real tarball bytes";
        let sri = sha512_sri(data);
        let digest = Sha512::digest(data);
        assert!(verify_sha512_integrity(&sri, &digest).is_ok());
    }

    #[test]
    fn verify_sha512_integrity_rejects_mismatch() {
        let sri = sha512_sri(b"the real tarball bytes");
        let tampered = Sha512::digest(b"a tampered tarball");
        assert!(verify_sha512_integrity(&sri, &tampered).is_err());
    }

    #[test]
    fn verify_sha512_integrity_prefers_sha512_among_multiple() {
        let data = b"payload";
        let sri = format!("sha256-AAAA {}", sha512_sri(data));
        let digest = Sha512::digest(data);
        assert!(verify_sha512_integrity(&sri, &digest).is_ok());
    }

    #[test]
    fn verify_sha512_integrity_skips_when_no_sha512_present() {
        // Only a (legacy) non-sha512 entry: nothing strong to verify against.
        assert!(verify_sha512_integrity("sha1-abc123", b"anything").is_ok());
    }

    #[test]
    fn list_remote_versions_parses_registry_response() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "versions": {
                        "2.1.90": {"dist": {"tarball": "https://example.com/2.1.90.tgz"}},
                        "2.1.91": {"dist": {"tarball": "https://example.com/2.1.91.tgz"}},
                        "2.1.92": {"dist": {"tarball": "https://example.com/2.1.92.tgz"}}
                    },
                    "dist-tags": {"latest": "2.1.92"}
                }"#,
            )
            .create();

        let versions = list_remote_versions_at(&server.url()).expect("success");
        assert_eq!(versions.len(), 3);
        assert_eq!(versions[0].to_string(), "2.1.90");
        assert_eq!(versions[2].to_string(), "2.1.92");
    }

    #[test]
    fn list_remote_versions_errors_on_500() {
        let mut server = Server::new();
        let _m = server.mock("GET", "/").with_status(500).create();
        let result = list_remote_versions_at(&server.url());
        assert!(result.is_err());
    }

    #[test]
    fn get_latest_version_returns_dist_tag() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "versions": {"2.1.92": {"dist": {"tarball": "https://example.com/2.1.92.tgz"}}},
                    "dist-tags": {"latest": "2.1.92"}
                }"#,
            )
            .create();

        let latest = get_latest_version_at(&server.url()).expect("success");
        assert_eq!(latest, "2.1.92");
    }

    // ── Download failure-path cleanup ────────────────────────────────

    /// A reader that yields its bytes once and then errors on the next read
    /// (never a clean EOF) — models a server that promised more via
    /// Content-Length and then dropped the connection mid-stream.
    struct InterruptedReader {
        remaining: Vec<u8>,
        failed: bool,
    }

    impl Read for InterruptedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.remaining.is_empty() {
                let take = self.remaining.len().min(buf.len());
                buf[..take].copy_from_slice(&self.remaining[..take]);
                self.remaining.drain(..take);
                return Ok(take);
            }
            if self.failed {
                return Ok(0);
            }
            self.failed = true;
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed mid-stream",
            ))
        }
    }

    #[test]
    fn interrupted_tarball_download_leaves_no_partial_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("raw").join("claude-code.tgz");
        std::fs::create_dir_all(dest.parent().unwrap()).expect("parent");

        let mut reader = InterruptedReader {
            remaining: vec![b'x'; 4096],
            failed: false,
        };
        let error = stream_tarball_to_file(
            &mut reader,
            &dest,
            None,
            "https://registry.test/claude.tgz",
            &ProgressBar::hidden(),
        )
        .expect_err("interrupted download must fail");

        assert!(error.to_string().contains("closed mid-stream"), "{error}");
        assert!(
            !dest.exists(),
            "the partially-written tarball must be removed"
        );
    }

    #[test]
    fn integrity_mismatch_removes_downloaded_tarball() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("raw").join("claude-code.tgz");
        std::fs::create_dir_all(dest.parent().unwrap()).expect("parent");

        // The bytes hash to their own SHA-512, but the registry integrity
        // metadata is for different content.
        let bytes = b"the delivered tarball bytes".to_vec();
        let wrong_sri = sha512_sri(b"what the registry expected");
        let mut reader = std::io::Cursor::new(bytes);

        let error = stream_tarball_to_file(
            &mut reader,
            &dest,
            Some(&wrong_sri),
            "https://registry.test/claude.tgz",
            &ProgressBar::hidden(),
        )
        .expect_err("integrity mismatch must fail");

        assert!(
            error.to_string().contains("does not match registry"),
            "{error}"
        );
        assert!(
            !dest.exists(),
            "the version dir must not keep a checksum-failed tarball"
        );
    }

    #[test]
    fn extract_tarball_rejects_oversized_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive_path = dir.path().join("bomb.tar.gz");
        let dest = dir.path().join("out");

        // A header claiming ~8 GiB (11 octal sevens) — above the 4 GiB cap —
        // with no data blocks behind it. Extraction must reject on the declared
        // size before it tries to read or allocate anything.
        create_tar_gz_with_declared_size(&archive_path, b"package/huge.bin", 0o77777777777);

        let error = extract_tarball(&archive_path, &dest).expect_err("oversized entry rejected");
        assert!(error.to_string().contains("oversized"), "{error}");
        assert!(!dest.join("package/huge.bin").exists());
    }

    #[test]
    fn extract_tarball_rejects_pax_size_override_bypass() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive_path = dir.path().join("pax-bomb.tar.gz");
        let dest = dir.path().join("out");

        // The raw ustar header declares a tiny 4 bytes (well under the cap) but a
        // preceding PAX extended header overrides the effective size to ~8 GiB.
        // The old code validated the raw header size and would have let this
        // through; validating the effective `Entry::size()` must reject it.
        create_tar_gz_with_pax_size(
            &archive_path,
            b"package/huge.bin",
            4,
            8 * 1024 * 1024 * 1024,
        );

        let error =
            extract_tarball(&archive_path, &dest).expect_err("PAX-oversized entry rejected");
        assert!(error.to_string().contains("oversized"), "{error}");
        assert!(!dest.join("package/huge.bin").exists());
    }

    fn ustar_block(name: &[u8], size: u64, typeflag: u8) -> [u8; 512] {
        let mut header = [0u8; 512];
        let len = name.len().min(99);
        header[..len].copy_from_slice(&name[..len]);
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size_str = format!("{size:011o}\0");
        header[124..136].copy_from_slice(size_str.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[156] = typeflag;
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].copy_from_slice(b"        ");
        let cksum: u32 = header.iter().map(|&b| b as u32).sum();
        let cksum_str = format!("{cksum:06o}\0 ");
        header[148..156].copy_from_slice(&cksum_str.as_bytes()[..8]);
        header
    }

    /// Build a single PAX extended-header record (`"<len> key=value\n"`), whose
    /// length prefix counts itself.
    fn pax_record(content: &str) -> String {
        let mut len = content.len() + 2;
        loop {
            let candidate = format!("{len} {content}");
            if candidate.len() == len {
                return candidate;
            }
            len = candidate.len();
        }
    }

    /// Write a tar.gz whose regular-file entry declares `header_size` in its raw
    /// ustar header but carries a PAX extended header overriding the effective
    /// size to `pax_size`. Used to prove the cap validates the effective size.
    fn create_tar_gz_with_pax_size(
        path: &std::path::Path,
        entry_name: &[u8],
        header_size: u64,
        pax_size: u64,
    ) {
        use std::io::Write;

        let records = pax_record(&format!("size={pax_size}\n"));
        let records = records.as_bytes();
        let pax_header = ustar_block(b"PaxHeaders/huge.bin", records.len() as u64, b'x');
        // PAX records padded up to the next 512-byte block boundary.
        let mut pax_body = records.to_vec();
        pax_body.resize(records.len().div_ceil(512) * 512, 0);

        let file_header = ustar_block(entry_name, header_size, b'0');
        // File body padded to a full block; content is irrelevant (rejected first).
        let mut file_body = vec![b'A'; header_size as usize];
        file_body.resize(512, 0);

        let end = [0u8; 1024];
        let file = std::fs::File::create(path).expect("create file");
        let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        gz.write_all(&pax_header).expect("write pax header");
        gz.write_all(&pax_body).expect("write pax body");
        gz.write_all(&file_header).expect("write file header");
        gz.write_all(&file_body).expect("write file body");
        gz.write_all(&end).expect("write end");
        gz.finish().expect("finish gzip");
    }

    /// Write a tar.gz with a single regular-file entry whose header *declares*
    /// `declared_size` bytes while carrying no data — used to exercise the
    /// size cap without producing a genuinely huge file.
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
        let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        gz.write_all(&header).expect("write header");
        gz.write_all(&end).expect("write end");
        gz.finish().expect("finish gzip");
    }

    #[test]
    fn list_remote_versions_skips_invalid_semver() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "versions": {
                        "not-a-version": {"dist": {"tarball": "x"}},
                        "2.1.91": {"dist": {"tarball": "https://example.com/2.1.91.tgz"}}
                    },
                    "dist-tags": {"latest": "2.1.91"}
                }"#,
            )
            .create();

        let versions = list_remote_versions_at(&server.url()).expect("success");
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].to_string(), "2.1.91");
    }
}
