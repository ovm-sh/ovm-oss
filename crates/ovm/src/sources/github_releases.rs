#[cfg(test)]
use crate::error::OvmError;
use crate::error::Result;
use crate::product::Product;
use serde::Deserialize;

const DEFAULT_GITHUB_API: &str = "https://api.github.com";

/// GitHub `owner/repo` slug for a given product's public releases.
pub fn github_repo(product: Product) -> &'static str {
    match product {
        Product::Claude => "anthropics/claude-code",
        Product::Codex => "openai/codex",
        Product::Pi => "earendil-works/pi",
    }
}

/// Resolve the GitHub API base URL. Tests set `OVM_GITHUB_API_URL` to point at a mock server.
fn api_base() -> String {
    std::env::var("OVM_GITHUB_API_URL").unwrap_or_else(|_| DEFAULT_GITHUB_API.to_string())
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    #[allow(dead_code)]
    tag_name: String,
    body: Option<String>,
    #[allow(dead_code)]
    published_at: Option<String>,
}

/// Info about a release from GitHub.
#[cfg(test)]
pub struct ReleaseInfo {
    pub version: String,
    pub date: Option<String>,
}

/// Fetch release notes for a specific product version from GitHub.
pub fn get_release_notes(product: Product, version: &str) -> Result<Option<String>> {
    get_release_notes_at_base(product, github_repo(product), version, &api_base())
}

fn get_release_notes_at_base(
    product: Product,
    repo: &str,
    version: &str,
    api_base: &str,
) -> Result<Option<String>> {
    let tag = format_tag(product, version);
    let url = format!("{api_base}/repos/{repo}/releases/tags/{tag}");

    let response = client()?.get(&url).send()?;

    if !response.status().is_success() {
        return Ok(None);
    }

    let release: GitHubRelease = response.json()?;
    Ok(release.body)
}

/// Fetch recent releases with dates and bodies for the given product.
#[cfg(test)]
pub fn get_recent_releases(product: Product, limit: usize) -> Result<Vec<ReleaseInfo>> {
    get_recent_releases_at_base(product, github_repo(product), limit, &api_base())
}

#[cfg(test)]
fn get_recent_releases_at_base(
    product: Product,
    repo: &str,
    limit: usize,
    api_base: &str,
) -> Result<Vec<ReleaseInfo>> {
    let url = format!("{api_base}/repos/{repo}/releases?per_page={limit}");

    let response = client()?.get(&url).send()?;

    if !response.status().is_success() {
        return Err(OvmError::Message(format!(
            "Failed to fetch releases: {}",
            response.status()
        )));
    }

    let releases: Vec<GitHubRelease> = response.json()?;

    Ok(releases
        .into_iter()
        .map(|r| ReleaseInfo {
            version: parse_tag(product, &r.tag_name),
            date: r.published_at.map(|d| {
                if d.len() >= 10 {
                    d[..10].to_string()
                } else {
                    d
                }
            }),
        })
        .collect())
}

fn format_tag(product: Product, version: &str) -> String {
    match product {
        Product::Claude | Product::Pi => {
            if version.starts_with('v') {
                version.to_string()
            } else {
                format!("v{version}")
            }
        }
        Product::Codex => version.to_string(),
    }
}

#[cfg(test)]
fn parse_tag(product: Product, tag: &str) -> String {
    match product {
        Product::Claude | Product::Pi => tag.strip_prefix('v').unwrap_or(tag).to_string(),
        Product::Codex => tag.to_string(),
    }
}

fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent("ovm")
        .timeout(std::time::Duration::from_secs(10))
        .redirect(super::https_only_redirect_policy())
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    #[test]
    fn formats_tags_per_product() {
        assert_eq!(format_tag(Product::Claude, "2.1.91"), "v2.1.91");
        assert_eq!(format_tag(Product::Claude, "v2.1.91"), "v2.1.91");
        assert_eq!(format_tag(Product::Pi, "0.67.6"), "v0.67.6");
        assert_eq!(format_tag(Product::Codex, "rust-v0.120.0"), "rust-v0.120.0");
    }

    #[test]
    fn parses_tags_per_product() {
        assert_eq!(parse_tag(Product::Claude, "v2.1.91"), "2.1.91");
        assert_eq!(parse_tag(Product::Pi, "v0.67.6"), "0.67.6");
        assert_eq!(parse_tag(Product::Codex, "rust-v0.120.0"), "rust-v0.120.0");
    }

    #[test]
    fn github_repo_maps_each_product() {
        assert_eq!(github_repo(Product::Claude), "anthropics/claude-code");
        assert_eq!(github_repo(Product::Codex), "openai/codex");
        assert_eq!(github_repo(Product::Pi), "earendil-works/pi");
    }

    #[test]
    fn get_release_notes_returns_body_on_success() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/repos/anthropics/claude-code/releases/tags/v2.1.91")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "tag_name": "v2.1.91",
                    "body": "Changelog body here - Added thing - Fixed bug",
                    "published_at": "2026-04-01T12:00:00Z"
                }"#,
            )
            .create();

        let notes = get_release_notes_at_base(
            Product::Claude,
            "anthropics/claude-code",
            "2.1.91",
            &server.url(),
        )
        .expect("success")
        .expect("has body");
        assert!(notes.contains("Changelog body here"));
        assert!(notes.contains("Added thing"));
    }

    #[test]
    fn get_release_notes_uses_codex_tag_verbatim() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/repos/openai/codex/releases/tags/rust-v0.120.0")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "tag_name": "rust-v0.120.0",
                    "body": "Codex notes",
                    "published_at": "2026-04-01T12:00:00Z"
                }"#,
            )
            .create();

        let notes = get_release_notes_at_base(
            Product::Codex,
            "openai/codex",
            "rust-v0.120.0",
            &server.url(),
        )
        .expect("success")
        .expect("has body");
        assert_eq!(notes, "Codex notes");
    }

    #[test]
    fn get_release_notes_returns_none_on_404() {
        let mut server = Server::new();
        let _m = server
            .mock(
                "GET",
                "/repos/anthropics/claude-code/releases/tags/v99.99.99",
            )
            .with_status(404)
            .create();

        let result = get_release_notes_at_base(
            Product::Claude,
            "anthropics/claude-code",
            "99.99.99",
            &server.url(),
        )
        .expect("no error on 404");
        assert!(result.is_none());
    }

    #[test]
    fn get_recent_releases_parses_list() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/repos/anthropics/claude-code/releases?per_page=50")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[
                    {"tag_name": "v2.1.91", "body": "note A", "published_at": "2026-04-01T12:00:00Z"},
                    {"tag_name": "v2.1.90", "body": null, "published_at": "2026-03-29T08:00:00Z"},
                    {"tag_name": "2.1.89", "body": null, "published_at": null}
                ]"#,
            )
            .create();

        let releases = get_recent_releases_at_base(
            Product::Claude,
            "anthropics/claude-code",
            50,
            &server.url(),
        )
        .expect("success");
        assert_eq!(releases.len(), 3);
        assert_eq!(releases[0].version, "2.1.91");
        assert_eq!(releases[0].date.as_deref(), Some("2026-04-01"));
        // v-prefix stripped
        assert_eq!(releases[1].version, "2.1.90");
        // No v-prefix to strip
        assert_eq!(releases[2].version, "2.1.89");
        assert_eq!(releases[2].date, None);
    }

    #[test]
    fn get_recent_releases_preserves_codex_tag_prefix() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/repos/openai/codex/releases?per_page=10")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[
                    {"tag_name": "rust-v0.120.0", "body": "note A", "published_at": "2026-04-01T12:00:00Z"}
                ]"#,
            )
            .create();

        let releases =
            get_recent_releases_at_base(Product::Codex, "openai/codex", 10, &server.url())
                .expect("success");
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].version, "rust-v0.120.0");
        assert_eq!(releases[0].date.as_deref(), Some("2026-04-01"));
    }

    #[test]
    fn get_recent_releases_errors_on_5xx() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(500)
            .create();

        let result = get_recent_releases_at_base(
            Product::Claude,
            "anthropics/claude-code",
            10,
            &server.url(),
        );
        assert!(result.is_err());
    }
}
