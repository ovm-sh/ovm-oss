use crate::claude_install::{self, ClaudeHygiene};
use crate::companions;
use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::version_manager::VersionManager;
use console::style;

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
    Ok(())
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
