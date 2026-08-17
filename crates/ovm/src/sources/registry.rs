use crate::product::Product;
use serde::Deserialize;
use std::collections::HashMap;

const DEFAULT_REGISTRY_BASE: &str = "https://ovm.sh/api";

/// Resolve the registry base URL. Tests set `OVM_REGISTRY_BASE_URL` to point at a mock server.
fn registry_base() -> String {
    std::env::var("OVM_REGISTRY_BASE_URL").unwrap_or_else(|_| DEFAULT_REGISTRY_BASE.to_string())
}

#[derive(Debug, Deserialize)]
struct ProductRegistry {
    versions: Vec<VersionEntry>,
}

#[derive(Debug, Deserialize)]
struct VersionEntry {
    version: String,
    date: Option<String>,
}

/// Versions + dates from the registry.
pub type VersionsWithDates = (Vec<String>, HashMap<String, String>);

/// Get version list + dates from the ovm.sh registry.
///
/// Returns `None` for any failure mode — connection error, non-2xx response,
/// malformed JSON, or a misbuilt HTTP client. The registry is an optimization,
/// not a source of truth, so callers always have a fallback path.
///
/// Set `OVM_VERBOSE=1` to surface the underlying reason on stderr.
pub fn list_versions_from_registry(product: Product) -> Option<VersionsWithDates> {
    list_versions_at_base(product, &registry_base())
}

/// A release's stamped asset list, or `None` if this version is not stamped.
///
/// Resolving a version's download URL from GitHub costs one API call against a
/// quota of 60/hour that is anonymous clients' only allowance and is counted
/// per IP address. At nightly volume that is invisible; during a bulk install
/// it is fatal — on 2026-08-14 three codex sweep windows spent the whole quota
/// and every install afterwards failed with 403. `scripts/stamp-release-assets.py`
/// mirrors each release's installable assets into the registry, so a stamped
/// version resolves with no GitHub request at all, for backfills and users alike.
///
/// `None` on every failure, exactly like the version listing above: this is an
/// optimization and the caller keeps its API path. The tag is validated before
/// it reaches the URL — it comes from the command line, and a version like
/// `../../etc` must not walk out of the registry's namespace.
pub fn release_manifest_from_registry<T: serde::de::DeserializeOwned>(
    product: Product,
    tag: &str,
) -> Option<T> {
    release_manifest_at_base(product, tag, &registry_base())
}

fn release_manifest_at_base<T: serde::de::DeserializeOwned>(
    product: Product,
    tag: &str,
    base: &str,
) -> Option<T> {
    let safe = !tag.is_empty()
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !safe {
        verbose_log(&format!(
            "refusing to build a registry URL from tag {tag:?}"
        ));
        return None;
    }
    let url = format!("{base}/{}/releases/{tag}.json", product.canonical_name());
    fetch_json(&url)
}

/// Testable core — accepts base URL directly so tests don't rely on env vars.
fn list_versions_at_base(product: Product, base: &str) -> Option<VersionsWithDates> {
    let slug = product.canonical_name();
    let url = format!("{base}/{slug}.json");
    let registry: ProductRegistry = fetch_json(&url)?;

    let mut versions = Vec::new();
    let mut dates = HashMap::new();
    for entry in registry.versions {
        if !product.is_official_remote_version(&entry.version) {
            continue;
        }
        if let Some(date) = entry.date {
            dates.insert(entry.version.clone(), date);
        }
        versions.push(entry.version);
    }

    Some((versions, dates))
}

/// GET a JSON document from the registry, or `None` with the reason logged.
fn fetch_json<T: serde::de::DeserializeOwned>(url: &str) -> Option<T> {
    let client = match reqwest::blocking::Client::builder()
        .user_agent("ovm")
        .timeout(std::time::Duration::from_secs(5))
        // Defense in depth: no secret is attached to registry requests, but a
        // compromised or misconfigured ovm.sh must not be able to redirect us to
        // a plaintext or cross-host URL. Same HTTPS-only, same-host policy the
        // other metadata clients use.
        .redirect(super::https_only_redirect_policy())
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            verbose_log(&format!("http client init failed for {url}: {error}"));
            return None;
        }
    };

    let response = match client.get(url).send() {
        Ok(response) => response,
        Err(error) => {
            verbose_log(&format!("request to {url} failed: {error}"));
            return None;
        }
    };

    if !response.status().is_success() {
        verbose_log(&format!("{url} returned {}", response.status()));
        return None;
    }

    match response.json() {
        Ok(value) => Some(value),
        Err(error) => {
            verbose_log(&format!("{url} returned unparseable JSON: {error}"));
            None
        }
    }
}

fn verbose_log(message: &str) {
    if std::env::var("OVM_VERBOSE").is_ok() {
        eprintln!("  [registry] {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    /// The tag becomes a URL path segment and arrives from the command line
    /// (`ovm install codex <version>`), so a traversal attempt must be refused
    /// outright rather than normalized — and refused before any request goes
    /// out, so a crafted version string cannot aim OVM at an arbitrary path on
    /// the registry host.
    #[test]
    fn a_traversing_tag_never_becomes_a_registry_request() {
        let mut server = Server::new();
        let any = server.mock("GET", mockito::Matcher::Any).create();

        for tag in ["../../etc/passwd", "rust-v1.0/../..", "", "a b", "x?y=1"] {
            let manifest: Option<serde_json::Value> =
                release_manifest_at_base(Product::Codex, tag, &server.url());
            assert!(manifest.is_none(), "{tag:?} must be refused");
        }
        assert!(!any.matched(), "no request may leave for an unsafe tag");
    }

    #[test]
    fn a_stamped_manifest_is_read_from_the_product_namespace() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/codex/releases/rust-v0.73.0.json")
            .with_status(200)
            .with_body(r#"{"tag_name":"rust-v0.73.0","assets":[]}"#)
            .create();

        let manifest: serde_json::Value =
            release_manifest_at_base(Product::Codex, "rust-v0.73.0", &server.url())
                .expect("stamped manifest resolves");
        assert_eq!(manifest["tag_name"], "rust-v0.73.0");
    }

    #[test]
    fn parses_valid_registry_response() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/claude.json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "versions": [
                        {"version": "2.1.90", "date": "2026-03-29"},
                        {"version": "2.1.91", "date": "2026-04-01"},
                        {"version": "2.1.92"}
                    ]
                }"#,
            )
            .create();

        let (versions, dates) =
            list_versions_at_base(Product::Claude, &server.url()).expect("registry returned data");
        assert_eq!(versions, vec!["2.1.90", "2.1.91", "2.1.92"]);
        assert_eq!(dates.get("2.1.90"), Some(&"2026-03-29".to_string()));
        assert_eq!(dates.get("2.1.91"), Some(&"2026-04-01".to_string()));
        assert_eq!(dates.get("2.1.92"), None);
    }

    #[test]
    fn filters_non_official_codex_registry_entries() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/codex.json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "versions": [
                        {"version": "codex-rs-deadbeef-1-rust-v0.0.2504301219", "date": "2025-04-30"},
                        {"version": "rusty-v8-v147.4.0", "date": "2026-03-20"},
                        {"version": "rust-v0.131.0-alpha.16", "date": "2026-05-14"}
                    ]
                }"#,
            )
            .create();

        let (versions, dates) =
            list_versions_at_base(Product::Codex, &server.url()).expect("registry returned data");
        assert_eq!(versions, vec!["rust-v0.131.0-alpha.16"]);
        assert_eq!(
            dates.get("rust-v0.131.0-alpha.16"),
            Some(&"2026-05-14".to_string())
        );
    }

    #[test]
    fn returns_none_on_404() {
        let mut server = Server::new();
        let _m = server.mock("GET", "/codex.json").with_status(404).create();
        assert!(list_versions_at_base(Product::Codex, &server.url()).is_none());
    }

    #[test]
    fn refuses_plaintext_cross_host_redirect() {
        // A compromised/misconfigured registry that 30x-redirects to a plaintext
        // or cross-host URL must not be followed — the request fails and the
        // registry lookup returns None (callers fall back to other sources).
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/claude.json")
            .with_status(302)
            .with_header("location", "http://evil.example.com/claude.json")
            .create();
        assert!(list_versions_at_base(Product::Claude, &server.url()).is_none());
    }

    #[test]
    fn returns_none_on_500() {
        let mut server = Server::new();
        let _m = server.mock("GET", "/pi.json").with_status(500).create();
        assert!(list_versions_at_base(Product::Pi, &server.url()).is_none());
    }

    #[test]
    fn returns_none_on_invalid_json() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/claude.json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("not valid json {{{")
            .create();
        assert!(list_versions_at_base(Product::Claude, &server.url()).is_none());
    }

    #[test]
    fn handles_empty_versions_list() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/pi.json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"versions": []}"#)
            .create();

        let (versions, dates) =
            list_versions_at_base(Product::Pi, &server.url()).expect("registry returned data");
        assert!(versions.is_empty());
        assert!(dates.is_empty());
    }
}
