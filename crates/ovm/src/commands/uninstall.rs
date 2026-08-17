use crate::error::{OvmError, Result};
use crate::version_manager::VersionManager;
use console::style;
use std::io::Write;

use super::format_bytes;

pub fn run(vm: &VersionManager, version: Option<&str>, all: bool, yes: bool) -> Result<()> {
    if all {
        return run_all(vm, yes);
    }
    // Clap enforces `required_unless_present = "all"`, so a missing version
    // can only mean --all, handled above.
    let version = version.expect("clap requires a version without --all");
    vm.uninstall(version)?;
    println!(
        "{} Uninstalled {} {}",
        style("✓").green(),
        vm.product().display_name(),
        vm.product().normalize_version(version)
    );
    Ok(())
}

/// Fully leave a product: every version, the active one included, plus the
/// selection state. Destructive and unprompted-for, so it previews exactly
/// what goes and demands a typed confirmation — the product's own name, so
/// the confirmation says what is being lost.
fn run_all(vm: &VersionManager, yes: bool) -> Result<()> {
    let product = vm.product();
    let versions = vm.list_installed()?;
    // Leftovers alone still warrant the sweep: an interrupted download's
    // partial tree is exactly what someone leaving a product wants gone, and
    // early-exiting on "no listed versions" would leave it forever.
    let leftovers = vm.unlisted_leftover_dirs()?;
    if versions.is_empty() && leftovers.is_empty() {
        println!("No {} versions are installed.", product.display_name());
        return Ok(());
    }

    let active = vm.current_version()?;
    eprintln!(
        "  {} This removes everything under {}'s version store and clears the active selection:",
        style("!").yellow(),
        product.display_name(),
    );
    for version in &versions {
        let marker = if active.as_deref() == Some(version.as_str()) {
            " (active)"
        } else {
            ""
        };
        eprintln!("      {}{}", version, style(marker).cyan());
    }
    for leftover in &leftovers {
        eprintln!("      {}{}", leftover, style(" (partial)").dim());
    }

    if !yes && !confirm(vm)? {
        eprintln!("  {} Nothing was removed.", style("→").dim());
        return Ok(());
    }

    let (count, freed) = vm.uninstall_all()?;
    if count > 0 {
        println!(
            "{} Uninstalled all {} {} version{}, freed {}",
            style("✓").green(),
            count,
            product.display_name(),
            if count == 1 { "" } else { "s" },
            format_bytes(freed)
        );
    } else {
        println!(
            "{} Removed {} leftover partial install director{}, freed {}",
            style("✓").green(),
            leftovers.len(),
            if leftovers.len() == 1 { "y" } else { "ies" },
            format_bytes(freed)
        );
    }
    // Truth over tidiness: an OLD ovm binary (predating the product-wide
    // install lock) can slip an install in during the removal. If one
    // appeared, say so instead of claiming a clean slate.
    let appeared = vm.list_installed()?;
    if appeared.is_empty() {
        eprintln!(
            "  {} {} is no longer managed by OVM — `{}` brings it back",
            style("→").dim(),
            product.display_name(),
            product.install_example("latest"),
        );
    } else {
        eprintln!(
            "  {} Note: another OVM process installed {} {} during the removal; it was preserved",
            style("!").yellow(),
            product.display_name(),
            appeared.join(", "),
        );
    }
    Ok(())
}

/// Ask before deleting, by having the user type the product's name. A
/// non-interactive shell can never answer, so it is told how to proceed
/// rather than being blocked on a prompt nobody will ever see — and a piped
/// "yes" is not a person confirming, so only the flag works there.
fn confirm(vm: &VersionManager) -> Result<bool> {
    use std::io::IsTerminal;

    let product = vm.product();
    let name = product.canonical_name();
    if !console::Term::stderr().is_term() || !std::io::stdin().is_terminal() {
        return Err(OvmError::Message(format!(
            "Refusing to uninstall every {} version without confirmation: this shell is not \
             interactive. Re-run `ovm uninstall {name} --all` in a terminal, or pass \
             `ovm uninstall {name} --all --yes`.",
            product.display_name()
        )));
    }

    eprint!(
        "  {} Type {} to confirm: ",
        style("?").yellow().bold(),
        style(name).bold()
    );
    let _ = std::io::stderr().flush();

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim() == name)
}
