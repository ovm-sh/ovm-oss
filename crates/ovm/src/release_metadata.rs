use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseInstallMetadata {
    #[serde(default = "default_kind")]
    pub kind: String,
    pub version: String,
    pub resolved_tag: String,
    pub asset_name: String,
    pub download_url: String,
    pub archive_sha256: String,
    pub installed_at: String,
}

fn default_kind() -> String {
    "release".to_string()
}

impl ReleaseInstallMetadata {
    pub fn new(
        version: impl Into<String>,
        resolved_tag: impl Into<String>,
        asset_name: impl Into<String>,
        download_url: impl Into<String>,
        archive_sha256: impl Into<String>,
    ) -> Self {
        Self {
            kind: default_kind(),
            version: version.into(),
            resolved_tag: resolved_tag.into(),
            asset_name: asset_name.into(),
            download_url: download_url.into(),
            archive_sha256: archive_sha256.into(),
            installed_at: installed_at_utc_string(),
        }
    }

    #[cfg(test)]
    pub fn read(path: &std::path::Path) -> crate::error::Result<Option<Self>> {
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(Some(serde_json::from_str(&contents)?))
    }
}

fn installed_at_utc_string() -> String {
    // `date -u` keeps this lightweight and avoids adding a full time crate for one field.
    let output = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::ReleaseInstallMetadata;
    use tempfile::tempdir;

    #[test]
    fn reads_missing_metadata_as_none() {
        let dir = tempdir().expect("tempdir");
        let metadata =
            ReleaseInstallMetadata::read(&dir.path().join("missing.json")).expect("read");
        assert_eq!(metadata, None);
    }

    #[test]
    fn serializes_expected_fields() {
        let metadata = ReleaseInstallMetadata::new(
            "rust-v0.120.0",
            "rust-v0.120.0",
            "codex-aarch64-apple-darwin.tar.gz",
            "https://github.com/openai/codex/releases/download/rust-v0.120.0/codex-aarch64-apple-darwin.tar.gz",
            "abc123",
        );

        assert_eq!(metadata.kind, "release");
        assert_eq!(metadata.version, "rust-v0.120.0");
        assert_eq!(metadata.resolved_tag, "rust-v0.120.0");
        assert_eq!(metadata.asset_name, "codex-aarch64-apple-darwin.tar.gz");
        assert_eq!(metadata.archive_sha256, "abc123");
        assert!(!metadata.installed_at.is_empty());
    }
}
