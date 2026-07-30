use crate::error::{OvmError, Result};
use indicatif::{ProgressBar, ProgressStyle};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;

const CDN_BASE: &str = "https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases";
const GCS_DOWNLOAD_HOSTS: &[&str] = &["storage.googleapis.com"];

#[derive(Debug, Deserialize, serde::Serialize)]
pub(crate) struct Manifest {
    pub version: String,
    #[serde(rename = "buildDate")]
    pub build_date: String,
    pub platforms: std::collections::HashMap<String, PlatformInfo>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub(crate) struct PlatformInfo {
    #[serde(default = "default_binary_name")]
    pub binary: String,
    pub checksum: String,
    pub size: u64,
}

fn default_binary_name() -> String {
    "claude".to_string()
}

fn platform_for(target_os: &str, target_arch: &str) -> Result<&'static str> {
    match (target_os, target_arch) {
        ("macos", "aarch64") => Ok("darwin-arm64"),
        ("macos", "x86_64") => Ok("darwin-x64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        ("linux", "x86_64") => Ok("linux-x64"),
        _ => Err(OvmError::DownloadFailed {
            url: CDN_BASE.to_string(),
            message: format!(
                "Unsupported platform: no Claude Code GCS binary for target_os={} target_arch={}",
                target_os, target_arch
            ),
        }),
    }
}

fn platform() -> Result<&'static str> {
    platform_for(std::env::consts::OS, std::env::consts::ARCH)
}

pub fn get_latest_version() -> Result<String> {
    let url = format!("{}/latest", CDN_BASE);
    let response = super::http_client(15)?.get(&url).send()?;

    if !response.status().is_success() {
        return Err(OvmError::DownloadFailed {
            url,
            message: format!("HTTP {}", response.status()),
        });
    }

    Ok(response.text()?.trim().to_string())
}

fn get_manifest(version: &str) -> Result<Manifest> {
    let url = format!("{}/{}/manifest.json", CDN_BASE, version);
    let response = super::http_client(15)?.get(&url).send()?;

    if !response.status().is_success() {
        return Err(OvmError::VersionNotFound(version.to_string()));
    }

    Ok(response.json()?)
}

pub fn download_binary(version: &str, dest: &Path) -> Result<()> {
    let manifest = get_manifest(version)?;
    let plat = platform()?;

    let platform_info = manifest
        .platforms
        .get(plat)
        .ok_or_else(|| OvmError::DownloadFailed {
            url: format!("{}/{}/{}/{}", CDN_BASE, version, plat, "claude"),
            message: format!("No binary available for platform {}", plat),
        })?;

    // The binary name comes from the (bucket-controlled) manifest and becomes
    // a URL path segment. Reject anything but a single safe filename so a
    // hostile manifest can't path-cross to another object even while staying
    // on the allowed host.
    if platform_info.binary.is_empty()
        || platform_info.binary.contains('/')
        || platform_info.binary.contains('\\')
        || platform_info.binary.contains("..")
    {
        return Err(OvmError::DownloadFailed {
            url: format!("{}/{}/{}", CDN_BASE, version, plat),
            message: format!("manifest binary name is unsafe: {}", platform_info.binary),
        });
    }
    let url = format!("{}/{}/{}/{}", CDN_BASE, version, plat, platform_info.binary);

    crate::util::ensure_parent_dir(dest)?;
    let temp_dest = dest.with_extension("part");
    let _ = std::fs::remove_file(&temp_dest);

    // GCS has no dev/test URL override; the CDN host is fixed, so loopback is
    // never a legitimate download target here.
    super::validate_download_url(&url, GCS_DOWNLOAD_HOSTS, false)?;
    let mut response = super::download_http_client(120, GCS_DOWNLOAD_HOSTS)?
        .get(&url)
        .send()
        .map_err(|e| OvmError::DownloadFailed {
            url: url.clone(),
            message: e.to_string(),
        })?;
    super::validate_download_url(response.url().as_str(), GCS_DOWNLOAD_HOSTS, false)?;

    if !response.status().is_success() {
        return Err(OvmError::DownloadFailed {
            url,
            message: format!("HTTP {}", response.status()),
        });
    }

    let progress = ProgressBar::new(platform_info.size);
    progress.set_style(
        ProgressStyle::default_bar()
            .template(
                "  {bar:36.cyan/dim} {bytes}/{total_bytes} ({percent}%) {bytes_per_sec} ETA {eta}",
            )
            .expect("valid progress template")
            .progress_chars("██░"),
    );

    let download_result = write_verified_binary(
        &mut response,
        &temp_dest,
        dest,
        platform_info,
        &progress,
        &url,
    );
    progress.finish_and_clear();
    download_result?;

    let manifest_path = dest
        .parent()
        .expect("dest has parent (ensured above)")
        .join("manifest.json");
    std::fs::write(manifest_path, serde_json::to_string_pretty(&manifest)?)?;

    Ok(())
}

/// Stream the download to the `.part` file, validate size/checksum, mark it
/// executable, verify the publisher signature, and atomically rename it into
/// place. Any failure removes the partial `.part` file so an interrupted or
/// corrupt download never leaves a stray artifact next to `dest`, and `dest`
/// itself is only created once every check passes.
fn write_verified_binary(
    reader: &mut impl Read,
    temp_dest: &Path,
    dest: &Path,
    platform_info: &PlatformInfo,
    progress: &ProgressBar,
    url: &str,
) -> Result<()> {
    let result = (|| -> Result<()> {
        let mut file = std::fs::File::create(temp_dest)?;
        stream_and_validate(reader, &mut file, platform_info, progress, url)?;
        crate::util::make_executable(temp_dest)?;
        super::verify_product_binary(crate::product::Product::Claude, temp_dest)?;
        std::fs::rename(temp_dest, dest)?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = std::fs::remove_file(temp_dest);
        return Err(error);
    }
    Ok(())
}

fn stream_and_validate(
    reader: &mut impl Read,
    writer: &mut impl Write,
    platform_info: &PlatformInfo,
    progress: &ProgressBar,
    url: &str,
) -> Result<()> {
    let mut buffer = [0u8; 8192];
    let mut downloaded = 0_u64;
    let mut hasher = Sha256::new();

    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .map_err(|error| OvmError::DownloadFailed {
                url: url.to_string(),
                message: error.to_string(),
            })?;
        if bytes_read == 0 {
            break;
        }

        writer.write_all(&buffer[..bytes_read])?;
        hasher.update(&buffer[..bytes_read]);
        downloaded += bytes_read as u64;
        progress.set_position(downloaded);
    }

    validate_download(platform_info, downloaded, &hasher.finalize())
}

fn validate_download(platform_info: &PlatformInfo, downloaded: u64, digest: &[u8]) -> Result<()> {
    if downloaded != platform_info.size {
        return Err(OvmError::DownloadFailed {
            url: "manifest".to_string(),
            message: format!(
                "Downloaded size mismatch: expected {} bytes, got {}",
                platform_info.size, downloaded
            ),
        });
    }

    let actual_checksum = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if actual_checksum != platform_info.checksum.to_ascii_lowercase() {
        return Err(OvmError::DownloadFailed {
            url: "manifest".to_string(),
            message: "Downloaded checksum mismatch".to_string(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        platform_for, stream_and_validate, validate_download, write_verified_binary, PlatformInfo,
    };
    use indicatif::ProgressBar;
    use sha2::{Digest, Sha256};
    use std::io::{Cursor, Read};

    /// A reader that yields its bytes once and then errors on the next read
    /// (never a clean EOF) — models a server that promised more via
    /// Content-Length and then dropped the connection mid-stream.
    struct InterruptedReader {
        remaining: Vec<u8>,
        failed: bool,
    }

    impl InterruptedReader {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                remaining: bytes,
                failed: false,
            }
        }
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

    fn platform_info(checksum: String, size: u64) -> PlatformInfo {
        PlatformInfo {
            binary: "claude".to_string(),
            checksum,
            size,
        }
    }

    #[test]
    fn platform_for_maps_supported_targets() {
        assert_eq!(
            platform_for("macos", "aarch64").expect("mac arm64"),
            "darwin-arm64"
        );
        assert_eq!(
            platform_for("macos", "x86_64").expect("mac x64"),
            "darwin-x64"
        );
        assert_eq!(
            platform_for("linux", "aarch64").expect("linux arm64"),
            "linux-arm64"
        );
        assert_eq!(
            platform_for("linux", "x86_64").expect("linux x64"),
            "linux-x64"
        );
    }

    #[test]
    fn platform_for_rejects_unsupported_targets() {
        let error = platform_for("windows", "x86_64").expect_err("unsupported");
        assert!(error.to_string().contains("Unsupported platform"));
        assert!(error.to_string().contains("target_os=windows"));
    }

    #[test]
    fn validate_download_accepts_matching_size_and_checksum() {
        let bytes = b"claude-binary";
        let checksum = Sha256::digest(bytes);
        let platform = PlatformInfo {
            binary: "claude".to_string(),
            checksum: checksum.iter().map(|byte| format!("{byte:02x}")).collect(),
            size: bytes.len() as u64,
        };

        assert!(validate_download(&platform, bytes.len() as u64, &checksum).is_ok());
    }

    #[test]
    fn validate_download_rejects_checksum_mismatch() {
        let bytes = b"claude-binary";
        let platform = PlatformInfo {
            binary: "claude".to_string(),
            checksum: "deadbeef".to_string(),
            size: bytes.len() as u64,
        };
        let checksum = Sha256::digest(bytes);

        let error = validate_download(&platform, bytes.len() as u64, &checksum)
            .expect_err("checksum mismatch");

        assert!(error.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn stream_updates_progress_and_preserves_bytes() {
        let bytes = vec![b'x'; 20_000];
        let checksum = Sha256::digest(&bytes);
        let platform = PlatformInfo {
            binary: "claude".to_string(),
            checksum: checksum.iter().map(|byte| format!("{byte:02x}")).collect(),
            size: bytes.len() as u64,
        };
        let progress = ProgressBar::hidden();
        let mut reader = Cursor::new(&bytes);
        let mut written = Vec::new();

        stream_and_validate(
            &mut reader,
            &mut written,
            &platform,
            &progress,
            "https://example.test/claude",
        )
        .expect("stream");

        assert_eq!(written, bytes);
        assert_eq!(progress.position(), platform.size);
    }

    #[test]
    fn interrupted_download_leaves_no_partial_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let version_dir = dir.path().join("versions").join("2.1.91");
        std::fs::create_dir_all(&version_dir).expect("version dir");
        let dest = version_dir.join("claude");
        let temp_dest = dest.with_extension("part");

        // The manifest promises 100 bytes; the "server" delivers 40 then drops.
        let platform = platform_info("deadbeef".to_string(), 100);
        let mut reader = InterruptedReader::new(vec![b'x'; 40]);

        let error = write_verified_binary(
            &mut reader,
            &temp_dest,
            &dest,
            &platform,
            &ProgressBar::hidden(),
            "https://example.test/claude",
        )
        .expect_err("interrupted download must fail");

        assert!(error.to_string().contains("closed mid-stream"), "{error}");
        assert!(!dest.exists(), "no binary should be published at dest");
        assert!(
            !temp_dest.exists(),
            "the partial .part file must be cleaned up"
        );
    }

    #[test]
    fn checksum_mismatch_removes_downloaded_artifact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let version_dir = dir.path().join("versions").join("2.1.91");
        std::fs::create_dir_all(&version_dir).expect("version dir");
        let dest = version_dir.join("claude");
        let temp_dest = dest.with_extension("part");

        // Correct size, but the manifest checksum does not match the bytes.
        let bytes = vec![b'z'; 64];
        let platform = platform_info("00".repeat(32), bytes.len() as u64);
        let mut reader = Cursor::new(bytes);

        let error = write_verified_binary(
            &mut reader,
            &temp_dest,
            &dest,
            &platform,
            &ProgressBar::hidden(),
            "https://example.test/claude",
        )
        .expect_err("checksum mismatch must fail");

        assert!(error.to_string().contains("checksum mismatch"), "{error}");
        assert!(!dest.exists(), "the version dir must not be half-populated");
        assert!(!temp_dest.exists(), "the partial artifact must be removed");
    }
}
