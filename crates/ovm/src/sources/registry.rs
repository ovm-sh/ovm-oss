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

/// Testable core — accepts base URL directly so tests don't rely on env vars.
fn list_versions_at_base(product: Product, base: &str) -> Option<VersionsWithDates> {
    let slug = product.canonical_name();
    let url = format!("{base}/{slug}.json");

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

    let response = match client.get(&url).send() {
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

    let registry: ProductRegistry = match response.json() {
        Ok(value) => value,
        Err(error) => {
            verbose_log(&format!("{url} returned unparseable JSON: {error}"));
            return None;
        }
    };

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

fn verbose_log(message: &str) {
    if std::env::var("OVM_VERBOSE").is_ok() {
        eprintln!("  [registry] {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

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
