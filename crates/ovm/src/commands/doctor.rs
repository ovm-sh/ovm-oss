use crate::claude_install::{self, ClaudeHygiene};
use crate::companions;
use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::version_manager::VersionManager;
use console::style;
use std::path::{Path, PathBuf};

/// `ovm doctor [product] [version] [--fix]` — report (and optionally repair)
/// known health issues:
///   * Claude install hygiene (launcher symlink + `~/.claude.json` settings) so
///     OVM stays the authoritative version manager.
///   * Whether the active version will run degraded against the live on-disk
///     state DB (a newer version may have applied a breaking migration that
///     removed a table this build still reads).
pub fn run(vm: &VersionManager, version: Option<&str>, fix: bool) -> Result<()> {
    let product = vm.product();

    if product == Product::Claude {
        check_claude_hygiene(fix)?;
        println!();
    } else if fix {
        println!(
            "  {} --fix only applies to Claude install hygiene; nothing to repair for {}.",
            style("·").dim(),
            product.display_name()
        );
    }

    if product.companions().is_empty() {
        println!(
            "{} has no shared schema store that ovm tracks — nothing to check.",
            product.display_name()
        );
        report_unmanaged_state_writers(product);
        return Ok(());
    }

    let version = match version {
        Some(value) => product.normalize_version(value),
        None => vm.current_version()?.ok_or(OvmError::NoActiveVersion)?,
    };
    let binary = vm.active_binary_path(&version);
    if !vm.install_is_complete(&version) {
        return Err(OvmError::Message(format!(
            "{} {version} is not installed, complete, or is archived. Run: {}",
            product.display_name(),
            product.install_example(&version)
        )));
    }

    let missing_companions = companions::missing(&vm.dirs, product);
    if !missing_companions.is_empty() {
        println!(
            "{} optional {} guard is not installed; skipping companion checks.",
            style("·").dim(),
            product.display_name()
        );
        for name in missing_companions {
            println!("  {} install with: cargo install {name}", style("→").cyan());
            println!(
                "  {} or place `{name}` at `{}`",
                style("→").cyan(),
                vm.dirs.base.join("companions").join(name).display()
            );
        }
        report_unmanaged_state_writers(product);
        return Ok(());
    }

    // Delegate the detailed schema-skew report to the product's optional
    // companion (e.g. Codex's `ovm-codex-skew`); it prints to stdout,
    // fail-open.
    companions::run(
        &vm.dirs,
        product,
        companions::Event::Doctor,
        &version,
        &binary,
    );
    report_unmanaged_state_writers(product);
    Ok(())
}

/// Installs that write the product's shared state but never appear on `PATH`.
///
/// `ovm adopt` already reports an unmanaged binary found on `PATH`, which
/// covers Homebrew and npm. It cannot see a GUI app bundle: nothing puts one
/// on `PATH`, so no part of OVM's view of the machine knows it is there.
///
/// That is the install worth naming. A desktop app updates itself on its own
/// schedule, so it can apply a breaking migration to the shared state DB with
/// no user action at all — which is exactly how a pinned build rots without
/// anyone touching it. The skew guard reports the aftermath; this says who
/// else can cause it.
fn app_bundle_candidates(product: Product, home: Option<&Path>) -> Vec<PathBuf> {
    if product != Product::Codex {
        return Vec::new();
    }
    let relative = Path::new("Applications/Codex.app/Contents/Resources/codex");
    let mut candidates = vec![Path::new("/").join(relative)];
    if let Some(home) = home {
        candidates.push(home.join(relative));
    }
    candidates
}

fn existing_state_writers(candidates: &[PathBuf]) -> Vec<PathBuf> {
    candidates
        .iter()
        .filter(|path| path.is_file())
        .cloned()
        .collect()
}

fn report_unmanaged_state_writers(product: Product) {
    let candidates = app_bundle_candidates(product, dirs::home_dir().as_deref());
    let found = existing_state_writers(&candidates);
    if found.is_empty() {
        return;
    }
    println!();
    println!(
        "  {} Also writes {}'s shared state, and is not on PATH so `ovm adopt` cannot see it:",
        style("!").yellow(),
        product.display_name()
    );
    for path in &found {
        println!("      {}", style(path.display()).dim());
    }
    println!(
        "  {} A desktop app updates itself, so it can migrate the shared state DB with no",
        style("·").dim()
    );
    println!(
        "  {} action from you. Keep it current, or remove it, so every writer moves together.",
        style("·").dim()
    );
}

/// Inspect — and with `fix`, repair — the OVM-managed Claude install so OVM
/// stays the authoritative version manager: flip `installMethod` off `native`
/// (the trigger for Claude's self-updater) and clear the `~/.local` native
/// install/launcher the updater otherwise keeps recreating.
fn check_claude_hygiene(fix: bool) -> Result<()> {
    let home = dirs::home_dir()
        .ok_or_else(|| OvmError::Config("Cannot determine home directory".into()))?;
    let hygiene = ClaudeHygiene::new(&home);

    let status = hygiene.inspect();
    claude_install::report(&status);

    if fix {
        if status.is_clean() {
            println!("  {} already clean — nothing to fix.", style("✓").green());
        } else {
            let actions = hygiene.apply()?;
            for action in &actions {
                println!("  {} {action}", style("→").cyan());
            }
            let after = hygiene.inspect();
            if after.is_clean() {
                println!("  {} repaired.", style("✓").green());
            } else {
                println!(
                    "  {} repaired what it could; re-run after `claude` has created ~/.claude.json.",
                    style("·").dim()
                );
            }
        }
    } else if !status.is_clean() {
        println!(
            "  {} run `ovm doctor claude --fix` to repair.",
            style("·").dim()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn only_codex_has_app_bundle_candidates() {
        assert!(app_bundle_candidates(Product::Claude, None).is_empty());
        assert!(!app_bundle_candidates(Product::Codex, None).is_empty());
    }

    #[test]
    fn a_home_relative_bundle_is_considered_too() {
        let home = Path::new("/Users/example");
        let candidates = app_bundle_candidates(Product::Codex, Some(home));
        assert!(candidates.iter().any(|path| path.starts_with(home)));
        assert!(candidates
            .iter()
            .any(|path| path.starts_with("/Applications")));
    }

    #[test]
    fn only_bundles_that_exist_are_reported() {
        let dir = tempdir().unwrap();
        let present = dir.path().join("codex");
        std::fs::write(&present, b"binary").unwrap();
        let absent = dir.path().join("missing");

        let found = existing_state_writers(&[present.clone(), absent]);

        assert_eq!(found, vec![present]);
    }
}
