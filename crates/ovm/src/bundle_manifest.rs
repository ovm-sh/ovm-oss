use crate::error::{OvmError, Result};
use std::collections::HashSet;
use std::path::Path;

pub const BUNDLE_MANIFEST_NAME: &str = "ovm-bundle-v1.tsv";
const BUNDLE_FORMAT: &str = "ovm-bundle-v1";
const EMBEDDED_MANIFEST: &str = include_str!("../ovm-bundle-v1.tsv");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleRole {
    Main,
    Side,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleEntry {
    pub role: BundleRole,
    pub binary: String,
    pub cargo_package: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleManifest {
    entries: Vec<BundleEntry>,
}

impl BundleManifest {
    pub fn parse(contents: &str) -> Result<Self> {
        let normalized = contents.replace("\r\n", "\n");
        let mut lines = normalized.lines();
        if lines.next() != Some(BUNDLE_FORMAT) {
            return Err(invalid("unsupported or missing format header"));
        }

        let mut entries = Vec::new();
        let mut binaries = HashSet::new();
        let mut packages = HashSet::new();
        let mut main_count = 0;

        for (index, line) in lines.enumerate() {
            let line_number = index + 2;
            let fields = line.split('\t').collect::<Vec<_>>();
            if fields.len() != 3 {
                return Err(invalid(format!(
                    "line {line_number} must contain exactly three tab-separated fields"
                )));
            }

            let role = match fields[0] {
                "main" => BundleRole::Main,
                "side" => BundleRole::Side,
                value => {
                    return Err(invalid(format!(
                        "line {line_number} has unknown role `{value}`"
                    )))
                }
            };
            let binary = fields[1];
            let cargo_package = fields[2];

            if !safe_binary_name(binary) {
                return Err(invalid(format!(
                    "line {line_number} has unsafe binary name `{binary}`"
                )));
            }
            if cargo_package != "-" && !safe_package_name(cargo_package) {
                return Err(invalid(format!(
                    "line {line_number} has unsafe Cargo package name `{cargo_package}`"
                )));
            }
            if !binaries.insert(binary.to_string()) {
                return Err(invalid(format!("duplicate binary `{binary}`")));
            }
            if cargo_package != "-" && !packages.insert(cargo_package.to_string()) {
                return Err(invalid(format!(
                    "duplicate Cargo package `{cargo_package}`"
                )));
            }

            if role == BundleRole::Main {
                main_count += 1;
                if binary != "ovm" || cargo_package != "ovm" {
                    return Err(invalid("the main row must be `main<TAB>ovm<TAB>ovm`"));
                }
            }

            entries.push(BundleEntry {
                role,
                binary: binary.to_string(),
                cargo_package: (cargo_package != "-").then(|| cargo_package.to_string()),
            });
        }

        if entries.is_empty() {
            return Err(invalid("manifest contains no binary rows"));
        }
        if main_count != 1 {
            return Err(invalid("manifest must contain exactly one main row"));
        }

        Ok(Self { entries })
    }

    pub fn load(path: &Path) -> Result<Self> {
        Self::parse(&std::fs::read_to_string(path)?)
    }

    pub fn embedded() -> Result<Self> {
        Self::parse(EMBEDDED_MANIFEST)
    }

    pub fn entries(&self) -> &[BundleEntry] {
        &self.entries
    }

    pub fn main(&self) -> &BundleEntry {
        self.entries
            .iter()
            .find(|entry| entry.role == BundleRole::Main)
            .expect("validated manifest has one main entry")
    }

    pub fn side_entries(&self) -> impl Iterator<Item = &BundleEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.role == BundleRole::Side)
    }

    pub fn binary_names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.binary.as_str())
    }

    pub fn to_tsv(&self) -> String {
        let mut output = format!("{BUNDLE_FORMAT}\n");
        for entry in &self.entries {
            let role = match entry.role {
                BundleRole::Main => "main",
                BundleRole::Side => "side",
            };
            let package = entry.cargo_package.as_deref().unwrap_or("-");
            output.push_str(&format!("{role}\t{}\t{package}\n", entry.binary));
        }
        output
    }
}

pub fn safe_binary_name(value: &str) -> bool {
    value == "ovm" || value.strip_prefix("ovm-").is_some_and(safe_dash_name)
}

fn safe_package_name(value: &str) -> bool {
    safe_dash_name(value)
}

fn safe_dash_name(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.as_bytes().windows(2).any(|pair| pair == b"--")
}

fn invalid(message: impl Into<String>) -> OvmError {
    OvmError::Message(format!("Invalid OVM bundle manifest: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_embedded_manifest() {
        let manifest = BundleManifest::embedded().unwrap();
        assert_eq!(manifest.main().binary, "ovm");
        assert_eq!(
            manifest.binary_names().collect::<Vec<_>>(),
            vec!["ovm", "ovm-codex-skew", "ovm-claudex"]
        );
        assert_eq!(manifest.to_tsv(), EMBEDDED_MANIFEST);
    }

    #[test]
    fn accepts_dynamic_side_binary_counts() {
        for sides in [0, 1, 4] {
            let mut contents = "ovm-bundle-v1\nmain\tovm\tovm\n".to_string();
            for index in 0..sides {
                contents.push_str(&format!("side\tovm-side-{index}\tovm-side-{index}\n"));
            }
            let manifest = BundleManifest::parse(&contents).unwrap();
            assert_eq!(manifest.side_entries().count(), sides);
        }
    }

    #[test]
    fn rejects_malformed_manifests() {
        let invalid = [
            "ovm-bundle-v2\nmain\tovm\tovm\n",
            "ovm-bundle-v1\n",
            "ovm-bundle-v1\nside\tovm-side\tovm-side\n",
            "ovm-bundle-v1\nmain\tovm-other\tovm\n",
            "ovm-bundle-v1\nmain\tovm\tovm\nside\t../ovm-side\tovm-side\n",
            "ovm-bundle-v1\nmain\tovm\tovm\nside\tovm-side\tovm-side\nside\tovm-side\tovm-other\n",
            "ovm-bundle-v1\nmain\tovm\tovm\nmain\tovm\tovm\n",
            "ovm-bundle-v1\nmain\tovm\tovm\nside\tovm--side\tovm-side\n",
        ];

        for contents in invalid {
            assert!(BundleManifest::parse(contents).is_err(), "{contents}");
        }
    }
}
