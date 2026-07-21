pub mod codex;
pub mod gcs;
pub mod github_releases;
pub mod npm;
pub mod pi;
pub mod registry;

pub(crate) fn http_client(timeout_secs: u64) -> crate::error::Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .user_agent("ovm")
        .redirect(https_only_redirect_policy())
        .build()?)
}

pub(crate) fn download_http_client(
    timeout_secs: u64,
    allowed_hosts: &[&str],
) -> crate::error::Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .user_agent("ovm")
        .redirect(download_redirect_policy(allowed_hosts))
        .build()?)
}

/// GitHub hosts that release assets are served from (the initial
/// `browser_download_url` is on `github.com`; it redirects to a release-asset
/// CDN host). Only the specific CDN hosts are listed — the bare
/// `githubusercontent.com` apex would also admit user-controlled content
/// hosts like `raw.githubusercontent.com`.
pub(crate) const GITHUB_DOWNLOAD_HOSTS: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// Verify a freshly downloaded product binary against the publisher's expected
/// Apple code-signing identity.
///
/// Anthropic and OpenAI ship Developer ID-signed, notarized macOS binaries, so
/// on macOS we confirm the signature is intact and the signing team matches the
/// expected publisher. This is the strongest provenance available for the
/// third-party tools ovm installs (they publish no build attestations).
///
/// No-op when the product ships no signed binaries (e.g. Pi) or on non-macOS
/// targets. `OVM_SKIP_SIGNATURE_VERIFY=1` bypasses verification; it exists for
/// tests and is deliberately not suggested in failure messages.
pub(crate) fn verify_product_binary(
    product: crate::product::Product,
    binary: &std::path::Path,
) -> crate::error::Result<()> {
    match product.expected_macos_team_id() {
        Some(team) => verify_macos_signature(binary, team),
        None => Ok(()),
    }
}

#[cfg(target_os = "macos")]
fn verify_macos_signature(
    binary: &std::path::Path,
    expected_team_id: &str,
) -> crate::error::Result<()> {
    use crate::error::OvmError;

    if std::env::var("OVM_SKIP_SIGNATURE_VERIFY").as_deref() == Ok("1") {
        return Ok(());
    }

    // 1. Signature is intact and satisfies its designated requirement.
    let verify = std::process::Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict"])
        .arg(binary)
        .output()
        .map_err(|e| OvmError::Message(format!("failed to run codesign: {e}")))?;
    if !verify.status.success() {
        return Err(OvmError::Message(format!(
            "code signature verification failed for {}: {}",
            binary.display(),
            String::from_utf8_lossy(&verify.stderr).trim()
        )));
    }

    // 2. Signing team matches the expected publisher.
    let display = std::process::Command::new("/usr/bin/codesign")
        .args(["--display", "--verbose=2"])
        .arg(binary)
        .output()
        .map_err(|e| OvmError::Message(format!("failed to run codesign: {e}")))?;
    // codesign writes its --display output to stderr.
    let info = String::from_utf8_lossy(&display.stderr);
    let team = info
        .lines()
        .find_map(|line| line.trim().strip_prefix("TeamIdentifier="))
        .map(str::trim);

    match team {
        Some(found) if found == expected_team_id => Ok(()),
        Some(found) => Err(OvmError::Message(format!(
            "unexpected code-signing team for {}: found `{found}`, expected `{expected_team_id}`. \
             The downloaded binary may have been tampered with; try again or report this at \
             https://github.com/ovm-sh/ovm/issues.",
            binary.display()
        ))),
        None => Err(OvmError::Message(format!(
            "{} is not code-signed (no TeamIdentifier). \
             The downloaded binary may have been tampered with; try again or report this at \
             https://github.com/ovm-sh/ovm/issues.",
            binary.display()
        ))),
    }
}

#[cfg(not(target_os = "macos"))]
fn verify_macos_signature(
    _binary: &std::path::Path,
    _expected_team_id: &str,
) -> crate::error::Result<()> {
    Ok(())
}

/// Redirect policy that follows redirects only while they remain on HTTPS.
///
/// Downloads start from an attacker-influenceable URL (release metadata,
/// registry responses). Without this, a redirect could silently downgrade a
/// download to plaintext HTTP or bounce it to an unexpected host over HTTP.
fn https_only_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        // Metadata (JSON) fetches must not cross hosts on redirect: a
        // compromised/misconfigured metadata host could otherwise bounce us to
        // an attacker host that returns forged asset URLs, checksums, or SRI.
        let same_host = attempt
            .previous()
            .last()
            .and_then(|prev| prev.host_str().map(str::to_owned))
            .zip(attempt.url().host_str())
            .map(|(prev, next)| prev == next)
            .unwrap_or(false);
        if attempt.url().scheme() != "https" {
            attempt.error("refusing to follow a non-HTTPS redirect")
        } else if attempt.previous().len() >= 10 {
            attempt.error("too many redirects")
        } else if !same_host {
            attempt.error("refusing to follow a cross-host metadata redirect")
        } else {
            attempt.follow()
        }
    })
}

fn download_redirect_policy(allowed_hosts: &[&str]) -> reqwest::redirect::Policy {
    let allowed_hosts = allowed_hosts
        .iter()
        .map(|host| normalize_host(host))
        .collect::<Vec<_>>();

    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }

        // Loopback is auto-allowed only when the request that issued this
        // redirect was *itself* loopback (a local mirror / test mock bouncing
        // between its own ports). A public allowed host — release metadata or a
        // CDN — must never redirect us to loopback: that would let a compromised
        // upstream drive an SSRF against local services. So loopback is allowed
        // as a redirect target only when the previous hop was loopback too.
        let previous_is_loopback = attempt
            .previous()
            .last()
            .and_then(|url| url.host_str())
            .map(is_loopback_host)
            .unwrap_or(false);

        match validate_download_url_parts(attempt.url(), &allowed_hosts, previous_is_loopback) {
            Ok(()) => attempt.follow(),
            Err(message) => attempt.error(message),
        }
    })
}

/// Validate that a download URL is safe to fetch.
///
/// Requires HTTPS and a host within `allowed_hosts` (exact match or a
/// subdomain of an allowed host). This guards against a compromised or MITM'd
/// metadata response redirecting a download to an internal or
/// attacker-controlled host.
///
/// `allow_loopback` permits a plain-HTTP loopback host (a local mirror or test
/// mock). It must be `true` ONLY when a dev/test `OVM_*_URL` override is in
/// effect (see [`test_override_active`]); in production it is `false`, so a
/// metadata host that hands back a loopback asset URL cannot drive an SSRF at
/// local services.
pub(crate) fn validate_download_url(
    url: &str,
    allowed_hosts: &[&str],
    allow_loopback: bool,
) -> crate::error::Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|e| crate::error::OvmError::DownloadFailed {
        url: url.to_string(),
        message: format!("invalid download URL: {e}"),
    })?;

    let allowed_hosts = allowed_hosts
        .iter()
        .map(|host| normalize_host(host))
        .collect::<Vec<_>>();

    // The initial download URL (never a redirect target) may be a loopback test
    // mock served over plain HTTP, but only when the caller's own test override
    // is active. Redirect targets are validated separately by
    // `download_redirect_policy`, which only allows loopback behind a loopback
    // origin.
    validate_download_url_parts(&parsed, &allowed_hosts, allow_loopback).map_err(|message| {
        crate::error::OvmError::DownloadFailed {
            url: url.to_string(),
            message,
        }
    })
}

/// Whether a dev/test URL override (`OVM_*_URL`) is set to a non-empty value.
///
/// A loopback download asset is a test-only affordance: a local mock server
/// serves the asset over plain HTTP on `127.0.0.1`. In production none of these
/// overrides are set, so a metadata response that points an asset at loopback
/// must be refused (otherwise a compromised-but-allowed metadata host could
/// drive an SSRF at local services). Download call sites pass the result of
/// this check as `allow_loopback` to [`validate_download_url`].
pub(crate) fn test_override_active(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|value| !value.is_empty())
}

/// `Url::host_str()` keeps the brackets on IPv6 literals.
fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

fn validate_download_url_parts(
    parsed: &reqwest::Url,
    allowed_hosts: &[String],
    allow_loopback: bool,
) -> std::result::Result<(), String> {
    let host = parsed
        .host_str()
        .ok_or_else(|| "download URL has no host".to_string())?;

    // Loopback over plain HTTP is only auto-allowed for the initial test-mock
    // request; as a redirect target it must land on an allowed host like any
    // other hop, so a public upstream cannot bounce a download to loopback.
    if allow_loopback && is_loopback_host(host) {
        return Ok(());
    }

    if parsed.scheme() != "https" {
        return Err(format!(
            "refusing non-HTTPS download URL (scheme `{}`)",
            parsed.scheme()
        ));
    }

    let host = normalize_host(host);
    let host_allowed = allowed_hosts.iter().any(|allowed| {
        !allowed.is_empty() && (host == *allowed || host.ends_with(&format!(".{allowed}")))
    });

    if !host_allowed {
        return Err(format!("download host `{host}` is not in the allowed set"));
    }

    Ok(())
}

fn normalize_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// Upper bound on the declared size of a single archive entry we will extract.
///
/// A hostile or corrupt archive can declare an enormous entry to exhaust disk
/// (a decompression/allocation bomb). The real tools OVM installs ship binaries
/// far below this bound, so 4 GiB is generous headroom while still bounding any
/// single extraction. Callers must pass the *effective* `Entry::size()` (which
/// reflects a PAX `size` extended-header override), not the raw header size, so
/// a PAX header cannot declare a small size to slip past the cap while the
/// entry's data reader streams a much larger body.
pub(crate) const MAX_TAR_ENTRY_SIZE: u64 = 4 * 1024 * 1024 * 1024;

/// Reject a tar entry whose effective size is larger than
/// [`MAX_TAR_ENTRY_SIZE`]. Pass `Entry::size()` (PAX-aware), not
/// `Entry::header().size()`: the `tar` crate limits the entry's data reader to
/// the effective size, so validating it rejects the entry before its
/// (potentially enormous) data is read or written to disk.
pub(crate) fn validate_tar_entry_size(
    size: u64,
    entry_path: &std::path::Path,
) -> crate::error::Result<()> {
    if size > MAX_TAR_ENTRY_SIZE {
        return Err(crate::error::OvmError::ExtractionFailed(format!(
            "archive entry {} declares an oversized {} bytes (max {})",
            entry_path.display(),
            size,
            MAX_TAR_ENTRY_SIZE
        )));
    }
    Ok(())
}

/// Validate that a tar entry path is safe to extract within `dest`.
///
/// Rejects paths containing `..` components (path traversal) and absolute paths.
/// Returns the full joined path on success.
pub(crate) fn validate_tar_entry_path(
    entry_path: &std::path::Path,
    dest: &std::path::Path,
) -> crate::error::Result<std::path::PathBuf> {
    use std::path::Component;

    for component in entry_path.components() {
        match component {
            Component::ParentDir => {
                return Err(crate::error::OvmError::ExtractionFailed(format!(
                    "path traversal detected in archive entry: {}",
                    entry_path.display()
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(crate::error::OvmError::ExtractionFailed(format!(
                    "absolute path in archive entry: {}",
                    entry_path.display()
                )));
            }
            _ => {}
        }
    }

    Ok(dest.join(entry_path))
}

#[cfg(test)]
mod tests {
    use super::{validate_download_url, validate_tar_entry_path, GITHUB_DOWNLOAD_HOSTS};
    use std::path::Path;

    #[cfg(target_os = "macos")]
    static SIGNATURE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn download_url_rejects_non_https() {
        assert!(
            validate_download_url("http://github.com/x", GITHUB_DOWNLOAD_HOSTS, false).is_err()
        );
        assert!(validate_download_url("ftp://github.com/x", GITHUB_DOWNLOAD_HOSTS, false).is_err());
    }

    #[test]
    fn download_url_rejects_unexpected_host() {
        assert!(
            validate_download_url("https://evil.example.com/x", GITHUB_DOWNLOAD_HOSTS, false)
                .is_err()
        );
        // Internal metadata endpoint over https is still rejected by the host check.
        assert!(
            validate_download_url("https://169.254.169.254/x", GITHUB_DOWNLOAD_HOSTS, false)
                .is_err()
        );
    }

    #[test]
    fn download_url_accepts_allowed_host_and_subdomains() {
        assert!(
            validate_download_url("https://github.com/a/b", GITHUB_DOWNLOAD_HOSTS, false).is_ok()
        );
        assert!(validate_download_url(
            "https://objects.githubusercontent.com/x",
            GITHUB_DOWNLOAD_HOSTS,
            false
        )
        .is_ok());
        assert!(validate_download_url(
            "https://release-assets.githubusercontent.com/x",
            GITHUB_DOWNLOAD_HOSTS,
            false
        )
        .is_ok());
    }

    #[test]
    fn download_url_rejects_user_content_githubusercontent_hosts() {
        // raw/gist serve arbitrary user-controlled content; only the
        // release-asset CDN hosts are trusted.
        assert!(validate_download_url(
            "https://raw.githubusercontent.com/attacker/repo/main/evil.tar.gz",
            GITHUB_DOWNLOAD_HOSTS,
            false
        )
        .is_err());
        assert!(validate_download_url(
            "https://gist.githubusercontent.com/attacker/x",
            GITHUB_DOWNLOAD_HOSTS,
            false
        )
        .is_err());
    }

    #[test]
    fn download_url_allows_loopback_over_http_only_with_override() {
        // With the test override active (allow_loopback = true) a loopback mock
        // over plain HTTP is accepted.
        assert!(validate_download_url("http://127.0.0.1:1234/asset", &[], true).is_ok());
        assert!(validate_download_url("http://localhost:1234/asset", &[], true).is_ok());
        // In production (no override, allow_loopback = false) a metadata-derived
        // loopback asset URL is refused — the SSRF guard.
        assert!(validate_download_url("http://127.0.0.1:1234/asset", &[], false).is_err());
        assert!(validate_download_url("http://localhost:1234/asset", &[], false).is_err());
    }

    #[test]
    fn download_client_rejects_redirect_to_unexpected_https_host_before_following() {
        let mut server = mockito::Server::new();
        let _redirect = server
            .mock("GET", "/asset")
            .with_status(302)
            .with_header("location", "https://169.254.169.254/latest/meta-data")
            .create();

        let error = super::download_http_client(5, GITHUB_DOWNLOAD_HOSTS)
            .expect("client")
            .get(format!("{}/asset", server.url()))
            .send()
            .expect_err("redirect should be rejected before follow");

        let debug = format!("{error:?}");
        assert!(debug.contains("download host"), "{debug}");
    }

    #[test]
    fn redirect_to_loopback_from_public_host_is_refused() {
        let allowed = [super::normalize_host("github.com")];
        let loopback = reqwest::Url::parse("http://127.0.0.1:9/latest/meta-data").unwrap();

        // A redirect issued by a public allowed host (previous hop not loopback)
        // must not be followed to a loopback address — the SSRF guard.
        assert!(super::validate_download_url_parts(&loopback, &allowed, false).is_err());
        // Even an HTTPS loopback target is refused: loopback is never in the
        // allowed set, so it fails the host check once auto-allow is off.
        let https_loopback = reqwest::Url::parse("https://127.0.0.1:9/x").unwrap();
        assert!(super::validate_download_url_parts(&https_loopback, &allowed, false).is_err());
        // A loopback origin (test mock) bouncing between its own ports is still
        // allowed, so the mock-server tests keep working.
        assert!(super::validate_download_url_parts(&loopback, &allowed, true).is_ok());
    }

    #[test]
    fn product_without_signing_team_skips_verification() {
        // Pi ships unsigned binaries, so verification is a no-op (even for a
        // path that does not exist) on every platform.
        let result = super::verify_product_binary(
            crate::product::Product::Pi,
            std::path::Path::new("/nonexistent/pi"),
        );
        assert!(result.is_ok());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn signature_skip_requires_explicit_one() {
        let _guard = SIGNATURE_ENV_LOCK.lock().expect("signature env lock");

        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("fake-binary");
        std::fs::write(&fake, b"#!/bin/sh\necho not a signed mach-o\n").expect("write");

        std::env::set_var("OVM_SKIP_SIGNATURE_VERIFY", "0");
        let zero_result = super::verify_macos_signature(&fake, "Q6L2SF6YDW");
        std::env::set_var("OVM_SKIP_SIGNATURE_VERIFY", "1");
        let one_result = super::verify_macos_signature(&fake, "Q6L2SF6YDW");
        std::env::remove_var("OVM_SKIP_SIGNATURE_VERIFY");

        assert!(zero_result.is_err());
        assert!(one_result.is_ok());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unsigned_binary_is_rejected_on_macos() {
        let _guard = SIGNATURE_ENV_LOCK.lock().expect("signature env lock");
        std::env::remove_var("OVM_SKIP_SIGNATURE_VERIFY");

        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("fake-binary");
        std::fs::write(&fake, b"#!/bin/sh\necho not a signed mach-o\n").expect("write");
        // An unsigned file fails the codesign --verify step.
        assert!(super::verify_macos_signature(&fake, "Q6L2SF6YDW").is_err());
    }

    #[test]
    fn rejects_parent_dir_traversal() {
        let dest = Path::new("/tmp/safe");
        assert!(validate_tar_entry_path(Path::new("../evil"), dest).is_err());
        assert!(validate_tar_entry_path(Path::new("a/../../evil"), dest).is_err());
    }

    #[test]
    fn rejects_absolute_paths() {
        let dest = Path::new("/tmp/safe");
        assert!(validate_tar_entry_path(Path::new("/etc/passwd"), dest).is_err());
    }

    #[test]
    fn accepts_safe_relative_paths() {
        let dest = Path::new("/tmp/safe");
        let result = validate_tar_entry_path(Path::new("package/index.js"), dest);
        assert_eq!(result.unwrap(), Path::new("/tmp/safe/package/index.js"));
    }

    #[test]
    fn accepts_current_dir_components() {
        let dest = Path::new("/tmp/safe");
        let result = validate_tar_entry_path(Path::new("./package/./file.js"), dest);
        assert!(result.is_ok());
    }
}
