//! Product-bound companion plugins.
//!
//! Some products can use an optional companion — a small native plugin OVM runs
//! automatically when present at lifecycle events (pre-launch, post-switch) and
//! for `ovm doctor`. Codex's companion `ovm-codex-skew` guards against running
//! a version degraded against a newer-migrated state DB.
//!
//! Unlike standalone `ovm-<name>` PATH plugins (user-invoked, discovered on
//! PATH), companions are resolved **deterministically** — never via PATH, so a
//! random PATH entry can't shadow the intended guard — and invoked with an env
//! contract. Running a companion is always best-effort and **fail-open**: a
//! missing or failing companion never blocks the launch or switch.

use crate::config::OvmDirs;
use crate::product::Product;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Copy)]
pub enum Event {
    PreLaunch,
    PostSwitch,
    Doctor,
}

impl Event {
    fn as_str(self) -> &'static str {
        match self {
            Event::PreLaunch => "pre-launch",
            Event::PostSwitch => "post-switch",
            Event::Doctor => "doctor",
        }
    }
}

/// Run every companion bound to `product` for `event`, passing the env contract
/// (`OVM_EVENT`/`OVM_PRODUCT`/`OVM_VERSION`/`OVM_BINARY`). Best-effort and
/// fail-open: spawn errors and non-zero exits are swallowed so a guard can never
/// block a launch or switch.
pub fn run(dirs: &OvmDirs, product: Product, event: Event, version: &str, binary: &Path) {
    for name in product.companions() {
        let Some(exe) = resolve(dirs, name) else {
            continue;
        };
        let _ = Command::new(exe)
            .env("OVM_EVENT", event.as_str())
            .env("OVM_PRODUCT", product.canonical_name())
            .env("OVM_VERSION", version)
            .env("OVM_BINARY", binary)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status();
    }
}

/// Optional companions configured for `product` but not installed in a location
/// OVM will use.
pub fn missing(dirs: &OvmDirs, product: Product) -> Vec<&'static str> {
    product
        .companions()
        .iter()
        .copied()
        .filter(|name| resolve(dirs, name).is_none())
        .collect()
}

/// Deterministically locate a companion binary by name. **Never** consults PATH.
/// Order: the installed companions dir (`~/.ovm/companions/`), then alongside the
/// running `ovm` executable (for distributions that choose to bundle companions).
fn resolve(dirs: &OvmDirs, name: &str) -> Option<PathBuf> {
    let installed = dirs.base.join("companions").join(name);
    if installed.exists() {
        return Some(installed);
    }
    let exe = std::env::current_exe().ok()?;
    let sibling = exe.parent()?.join(name);
    sibling.exists().then_some(sibling)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn no_companions_is_a_noop() {
        let dir = tempdir().unwrap();
        let dirs = OvmDirs::at(dir.path().to_path_buf());
        // Claude has no companions — must not panic or spawn anything.
        run(
            &dirs,
            Product::Claude,
            Event::PreLaunch,
            "1.0.0",
            Path::new("/bin/true"),
        );
    }

    #[test]
    fn resolve_prefers_installed_companions_dir() {
        let dir = tempdir().unwrap();
        let dirs = OvmDirs::at(dir.path().to_path_buf());
        let companions = dir.path().join("companions");
        std::fs::create_dir_all(&companions).unwrap();
        let target = companions.join("ovm-codex-skew");
        std::fs::write(&target, "#!/bin/sh\n").unwrap();

        assert_eq!(resolve(&dirs, "ovm-codex-skew"), Some(target));
    }

    #[test]
    fn resolve_returns_none_when_absent() {
        let dir = tempdir().unwrap();
        let dirs = OvmDirs::at(dir.path().to_path_buf());
        // A name that won't exist next to the test runner exe either.
        assert_eq!(resolve(&dirs, "ovm-nonexistent-companion-xyz"), None);
    }

    #[test]
    fn reports_missing_optional_companions() {
        let dir = tempdir().unwrap();
        let dirs = OvmDirs::at(dir.path().to_path_buf());

        assert_eq!(missing(&dirs, Product::Codex), vec!["ovm-codex-skew"]);
    }
}
