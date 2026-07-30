use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Product {
    Claude,
    Codex,
    Pi,
}

impl Product {
    pub const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Pi];

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "claude" | "cc" => Some(Self::Claude),
            "codex" | "cx" => Some(Self::Codex),
            "pi" => Some(Self::Pi),
            _ => None,
        }
    }

    pub fn canonical_name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Pi => "Pi",
        }
    }

    pub fn binary_name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
        }
    }

    /// Optional companion plugins OVM runs automatically for this product when
    /// installed. Resolved deterministically by [`crate::companions`], never via
    /// PATH. Codex can use `ovm-codex-skew`, which guards against running a
    /// version degraded against a newer-migrated state DB.
    pub fn companions(self) -> &'static [&'static str] {
        match self {
            Self::Codex => &["ovm-codex-skew"],
            Self::Claude | Self::Pi => &[],
        }
    }

    /// Apple Developer ID Team Identifier that signs this product's official
    /// macOS binaries, if any. Used to verify downloaded binaries via
    /// `codesign`. `None` means the product does not ship signed binaries
    /// (e.g. Pi), so no signature check is performed.
    ///
    /// Verified against real release binaries:
    /// - Claude Code: `Anthropic PBC`
    /// - Codex: `OpenAI OpCo, LLC`
    pub fn expected_macos_team_id(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("Q6L2SF6YDW"),
            Self::Codex => Some("2DC432GLL2"),
            Self::Pi => None,
        }
    }

    pub fn supports_npm(self) -> bool {
        matches!(self, Self::Claude)
    }

    pub fn supports_dev_installs(self) -> bool {
        matches!(self, Self::Codex)
    }

    pub fn install_example(self, version: &str) -> String {
        format!("ovm install {} {version}", self.canonical_name())
    }

    pub fn use_example(self, version: &str) -> String {
        format!("ovm use {} {version}", self.canonical_name())
    }

    pub fn normalize_version(self, value: &str) -> String {
        match self {
            Self::Claude => value.to_string(),
            Self::Pi => normalize_pi_version(value),
            Self::Codex => normalize_codex_version(value),
        }
    }

    pub fn sort_versions(self, versions: &mut [String]) {
        versions.sort_by(|left, right| self.compare_versions(left, right));
    }

    pub fn is_newer(self, candidate: &str, baseline: &str) -> bool {
        self.compare_versions(candidate, baseline) == Ordering::Greater
    }

    pub(crate) fn compare_version_strings(self, left: &str, right: &str) -> Ordering {
        self.compare_versions(left, right)
    }

    pub fn shortest_alias(self) -> &'static str {
        match self {
            Self::Claude => "cc",
            Self::Codex => "cx",
            Self::Pi => "pi",
        }
    }

    pub fn parsed_release_version(self, value: &str) -> Option<semver::Version> {
        self.parse_semver(value)
    }

    pub fn is_official_remote_version(self, value: &str) -> bool {
        match self {
            Self::Claude | Self::Pi => semver::Version::parse(value).is_ok(),
            Self::Codex => value
                .strip_prefix("rust-v")
                .and_then(|version| semver::Version::parse(version).ok())
                .is_some(),
        }
    }

    pub fn is_release_version(self, value: &str) -> bool {
        self.parse_semver(value)
            .is_some_and(|version| version.pre.is_empty())
    }

    fn compare_versions(self, left: &str, right: &str) -> Ordering {
        if matches!(self, Self::Codex) {
            match (left.starts_with("dev:"), right.starts_with("dev:")) {
                (true, true) => return left.cmp(right),
                (true, false) => return Ordering::Less,
                (false, true) => return Ordering::Greater,
                (false, false) => {}
            }
        }

        match (self.parse_semver(left), self.parse_semver(right)) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => left.cmp(right),
        }
    }

    fn parse_semver(self, value: &str) -> Option<semver::Version> {
        match self {
            Self::Claude | Self::Pi => semver::Version::parse(value).ok(),
            Self::Codex => {
                let trimmed = value
                    .strip_prefix("rust-v")
                    .or_else(|| value.strip_prefix('v'))
                    .unwrap_or(value);
                semver::Version::parse(trimmed).ok()
            }
        }
    }
}

fn normalize_pi_version(value: &str) -> String {
    if value == "latest" {
        return value.to_string();
    }

    if let Some(stripped) = value.strip_prefix('v') {
        return stripped.to_string();
    }

    value.to_string()
}

fn normalize_codex_version(value: &str) -> String {
    if value == "latest" || value.starts_with("rust-") {
        return value.to_string();
    }

    if let Some(stripped) = value.strip_prefix('v') {
        return format!("rust-v{stripped}");
    }

    if semver::Version::parse(value).is_ok() {
        return format!("rust-v{value}");
    }

    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::Product;

    #[test]
    fn parses_aliases() {
        assert_eq!(Product::parse("claude"), Some(Product::Claude));
        assert_eq!(Product::parse("cc"), Some(Product::Claude));
        assert_eq!(Product::parse("codex"), Some(Product::Codex));
        assert_eq!(Product::parse("cx"), Some(Product::Codex));
        assert_eq!(Product::parse("pi"), Some(Product::Pi));
        assert_eq!(Product::parse("other"), None);
    }

    #[test]
    fn normalizes_codex_release_versions() {
        assert_eq!(Product::Codex.normalize_version("0.118.0"), "rust-v0.118.0");
        assert_eq!(
            Product::Codex.normalize_version("v0.118.0"),
            "rust-v0.118.0"
        );
        assert_eq!(
            Product::Codex.normalize_version("rust-v0.118.0"),
            "rust-v0.118.0"
        );
        assert_eq!(Product::Codex.normalize_version("latest"), "latest");
        assert_eq!(Product::Claude.normalize_version("2.1.91"), "2.1.91");
    }

    #[test]
    fn codex_official_remote_versions_are_rust_release_tags_only() {
        assert!(Product::Codex.is_official_remote_version("rust-v0.131.0-alpha.16"));
        assert!(Product::Codex.is_official_remote_version("rust-v0.130.0"));
        assert!(!Product::Codex.is_official_remote_version(
            "codex-rs-b289c9207090b2e27494545d7b5404e063bd86f3-1-rust-v0.1.0-alpha.4"
        ));
        assert!(!Product::Codex.is_official_remote_version("rusty-v8-v147.4.0"));
        assert!(!Product::Codex.is_official_remote_version("v0.130.0"));
    }

    #[test]
    fn release_versions_exclude_prereleases() {
        assert!(Product::Codex.is_release_version("rust-v0.130.0"));
        assert!(!Product::Codex.is_release_version("rust-v0.131.0-alpha.16"));
        assert!(Product::Claude.is_release_version("2.1.91"));
        assert!(!Product::Claude.is_release_version("2.1.92-beta.1"));
    }

    #[test]
    fn sorts_codex_dev_versions_before_releases() {
        let mut versions = vec![
            "rust-v0.130.0".to_string(),
            "dev:thread-unsubscribe".to_string(),
            "rust-v0.129.0".to_string(),
            "dev:resume".to_string(),
        ];

        Product::Codex.sort_versions(&mut versions);

        assert_eq!(
            versions,
            vec![
                "dev:resume",
                "dev:thread-unsubscribe",
                "rust-v0.129.0",
                "rust-v0.130.0"
            ]
        );
    }

    #[test]
    fn normalizes_pi_versions() {
        assert_eq!(Product::Pi.normalize_version("v0.67.6"), "0.67.6");
        assert_eq!(Product::Pi.normalize_version("0.67.6"), "0.67.6");
        assert_eq!(
            Product::Pi.normalize_version("v1.2.3-beta.1"),
            "1.2.3-beta.1"
        );
        assert_eq!(Product::Pi.normalize_version("latest"), "latest");
    }
}
