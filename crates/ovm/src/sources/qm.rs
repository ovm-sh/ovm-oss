use crate::error::{OvmError, Result};
use serde::Serialize;
use std::path::Path;

const DEFAULT_NPM_PACKAGE_URL: &str = "https://registry.npmjs.org/@yc-software/qm";
const PACKAGE_NAME: &str = "@yc-software/qm";
const NPM_OVERRIDE: &str = "OVM_QM_NPM_PACKAGE_URL";

pub(crate) const ENTRYPOINT_PATH: &str = "package/dist/bin/qm.js";
pub(crate) const REQUIRED_FILE_PATHS: &[&str] = &["package/package.json"];
pub(crate) const REQUIRED_DIR_PATHS: &[&str] = &["package/dist"];

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QmInstallMetadata {
    kind: &'static str,
    version: String,
    package: &'static str,
    source: &'static str,
}

fn package_url() -> String {
    std::env::var(NPM_OVERRIDE).unwrap_or_else(|_| DEFAULT_NPM_PACKAGE_URL.to_string())
}

pub fn list_remote_versions() -> Result<Vec<String>> {
    Ok(super::npm::list_remote_versions_at(&package_url())?
        .into_iter()
        .map(|version| version.to_string())
        .collect())
}

pub fn get_latest_version() -> Result<String> {
    get_latest_version_at(&package_url())
}

fn get_latest_version_at(url: &str) -> Result<String> {
    let latest = super::npm::get_latest_version_at(url)?;
    let parsed =
        semver::Version::parse(&latest).map_err(|_| OvmError::VersionNotFound(latest.clone()))?;
    if !parsed.pre.is_empty() {
        return Err(OvmError::VersionNotFound(latest));
    }
    Ok(parsed.to_string())
}

pub fn download_release(version: &str, bundle_dir: &Path) -> Result<QmInstallMetadata> {
    std::fs::create_dir_all(bundle_dir)?;
    let archive_path = bundle_dir.join(format!(".qm-{version}.tgz"));
    let url = package_url();

    // npm publishes SHA-512 SRI for QM. If that field cannot be read, refusing
    // is intentional: an incomplete metadata response is not evidence that the
    // publisher shipped no digest.
    super::npm::download_tarball_to(&url, NPM_OVERRIDE, version, &archive_path, true)?;
    let extract_result = super::npm::extract_tarball(&archive_path, bundle_dir);
    let _ = std::fs::remove_file(&archive_path);
    extract_result?;

    let entrypoint = bundle_dir.join(ENTRYPOINT_PATH);
    if !entrypoint.is_file() {
        return Err(OvmError::ExtractionFailed(format!(
            "the {PACKAGE_NAME} package unpacked completely but contains no {ENTRYPOINT_PATH} entrypoint"
        )));
    }
    for required in REQUIRED_FILE_PATHS {
        if !bundle_dir.join(required).is_file() {
            return Err(OvmError::ExtractionFailed(format!(
                "the {PACKAGE_NAME} package unpacked completely but contains no required {required} file"
            )));
        }
    }
    for required in REQUIRED_DIR_PATHS {
        if !bundle_dir.join(required).is_dir() {
            return Err(OvmError::ExtractionFailed(format!(
                "the {PACKAGE_NAME} package unpacked completely but contains no required {required} directory"
            )));
        }
    }
    crate::util::make_executable(&entrypoint)?;

    Ok(QmInstallMetadata {
        kind: "release",
        version: version.to_string(),
        package: PACKAGE_NAME,
        source: "npm",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use sha2::{Digest, Sha512};
    use std::io::Write as _;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct OverrideGuard;

    impl Drop for OverrideGuard {
        fn drop(&mut self) {
            std::env::remove_var(NPM_OVERRIDE);
        }
    }

    fn package_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let encoder = flate2::write::GzEncoder::new(&mut bytes, flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            for (name, contents) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(contents.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, name, &contents[..])
                    .expect("append archive entry");
            }
            let encoder = builder.into_inner().expect("finish tar");
            encoder.finish().expect("finish gzip");
        }
        bytes
    }

    fn traversal_archive() -> Vec<u8> {
        let mut header = [0_u8; 512];
        let name = b"../outside";
        header[..name.len()].copy_from_slice(name);
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(b"00000000001\0");
        header[136..148].copy_from_slice(b"00000000000\0");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].copy_from_slice(b"        ");
        let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        let encoded = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(&encoded.as_bytes()[..8]);

        let mut bytes = Vec::new();
        let mut gzip = flate2::write::GzEncoder::new(&mut bytes, flate2::Compression::default());
        gzip.write_all(&header).expect("write header");
        let mut block = [0_u8; 512];
        block[0] = b'x';
        gzip.write_all(&block).expect("write body");
        gzip.write_all(&[0_u8; 1024]).expect("write trailer");
        gzip.finish().expect("finish gzip");
        bytes
    }

    fn sri(bytes: &[u8]) -> String {
        format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes))
        )
    }

    fn mock_package(
        archive: &[u8],
        integrity: &str,
        content_length: Option<usize>,
    ) -> (mockito::ServerGuard, Vec<mockito::Mock>) {
        let mut server = mockito::Server::new();
        let tarball_url = format!("{}/qm.tgz", server.url());
        let metadata = serde_json::json!({
            "dist": {"tarball": tarball_url, "integrity": integrity}
        });
        let version = server
            .mock("GET", "/0.1.4")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(metadata.to_string())
            .create();
        let mut tarball = server.mock("GET", "/qm.tgz").with_status(200);
        if let Some(length) = content_length {
            tarball = tarball.with_header("content-length", &length.to_string());
        }
        let tarball = tarball.with_body(archive).create();
        (server, vec![version, tarball])
    }

    fn with_mock_package(
        archive: &[u8],
        integrity: &str,
        content_length: Option<usize>,
        run: impl FnOnce(&Path) -> Result<QmInstallMetadata>,
    ) -> Result<QmInstallMetadata> {
        let (_env, _override_guard) = (
            ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner()),
            OverrideGuard,
        );
        let (server, _mocks) = mock_package(archive, integrity, content_length);
        std::env::set_var(NPM_OVERRIDE, server.url());
        let dir = tempfile::tempdir().expect("tempdir");
        run(dir.path())
    }

    #[test]
    fn downloads_complete_qm_bundle() {
        let archive = package_archive(&[
            ("package/package.json", br#"{"name":"@yc-software/qm"}"#),
            ("package/dist/bin/qm.js", b"#!/usr/bin/env node\n"),
        ]);
        with_mock_package(&archive, &sri(&archive), Some(archive.len()), |dest| {
            let metadata = download_release("0.1.4", dest)?;
            assert!(dest.join(ENTRYPOINT_PATH).is_file());
            assert!(dest.join("package/package.json").is_file());
            Ok(metadata)
        })
        .expect("complete package installs");
    }

    #[test]
    fn rejects_integrity_mismatch() {
        let archive = package_archive(&[("package/dist/bin/qm.js", b"x")]);
        let error = with_mock_package(&archive, &sri(b"different"), Some(archive.len()), |dest| {
            download_release("0.1.4", dest)
        })
        .expect_err("digest mismatch");
        assert!(
            error.to_string().contains("does not match registry"),
            "{error}"
        );
    }

    #[test]
    fn rejects_truncated_body() {
        let archive = package_archive(&[("package/dist/bin/qm.js", b"x")]);
        let truncated = &archive[..archive.len() / 2];
        let error = with_mock_package(truncated, &sri(&archive), None, |dest| {
            download_release("0.1.4", dest)
        })
        .expect_err("short body");
        assert!(
            error.to_string().contains("does not match registry"),
            "{error}"
        );
    }

    #[test]
    fn rejects_path_traversal_archive() {
        let archive = traversal_archive();
        let error = with_mock_package(&archive, &sri(&archive), Some(archive.len()), |dest| {
            download_release("0.1.4", dest)
        })
        .expect_err("path traversal");
        assert!(error.to_string().contains("path traversal"), "{error}");
    }

    #[test]
    fn rejects_bundle_missing_entrypoint() {
        let archive = package_archive(&[("package/package.json", b"{}")]);
        let error = with_mock_package(&archive, &sri(&archive), Some(archive.len()), |dest| {
            download_release("0.1.4", dest)
        })
        .expect_err("missing entrypoint");
        assert!(error.to_string().contains(ENTRYPOINT_PATH), "{error}");
    }

    #[test]
    fn rejects_latest_prerelease() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"versions":{"0.2.0-beta.1":{"dist":{"tarball":"https://example.invalid/qm.tgz"}}},"dist-tags":{"latest":"0.2.0-beta.1"}}"#,
            )
            .create();
        let error = get_latest_version_at(&server.url());
        assert!(error.is_err());
    }
}
