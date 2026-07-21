use crate::error::Result;
use crate::version_manager::VersionManager;
use console::style;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Just locally installed versions (default).
    Installed,
    /// Versions available to download (from the registry).
    Remote,
    /// Both — installed versions interleaved with remote.
    All,
}

pub fn run(vm: &VersionManager, scope: Scope) -> Result<()> {
    match scope {
        Scope::Installed => list_installed(vm),
        Scope::Remote => list_remote(vm, false),
        Scope::All => list_remote(vm, true),
    }
}

fn list_installed(vm: &VersionManager) -> Result<()> {
    let versions = vm.list_installed()?;
    let current = vm.current_version()?;

    if versions.is_empty() {
        println!(
            "No {} versions installed. Run: {}",
            vm.product().canonical_name(),
            vm.product().install_example("latest")
        );
        return Ok(());
    }

    for name in &versions {
        let sources = vm.version_sources(name);
        let source = if sources.is_empty() {
            style("[unknown]").dim().to_string()
        } else {
            let labels = sources
                .iter()
                .map(|source| source.label())
                .collect::<Vec<_>>()
                .join("+");
            style(format!("[{labels}]")).dim().to_string()
        };

        if current.as_deref() == Some(name.as_str()) {
            println!(
                "  {} {} {}",
                style("->").green().bold(),
                style(name).green().bold(),
                source
            );
        } else {
            println!("     {name} {source}");
        }
    }

    println!(
        "\n{} {} version(s) installed",
        versions.len(),
        vm.product().canonical_name()
    );
    Ok(())
}

/// List versions available in the remote registry.
/// If `include_installed_marker` is true, installed versions are shown with a ✓
/// and the active one is highlighted — same data as --all.
fn list_remote(vm: &VersionManager, _include_installed_marker: bool) -> Result<()> {
    let versions = vm.list_remote_versions()?;
    let installed_list = vm.list_installed()?;
    let installed: HashSet<&str> = installed_list.iter().map(String::as_str).collect();
    let current = vm.current_version()?;

    for version in &versions {
        let is_current = current.as_deref() == Some(version.as_str());

        if is_current {
            println!(
                "  {} {}",
                style("->").green().bold(),
                style(version).green().bold()
            );
        } else if installed.contains(version.as_str()) {
            println!("   * {}", style(version));
        } else {
            println!("     {}", style(version).dim());
        }
    }

    println!(
        "\n{} remote {} version(s), {} installed locally",
        versions.len(),
        vm.product().canonical_name(),
        installed.len()
    );

    if vm.product().supports_npm() {
        println!("  Native binaries available from 1.0.37+ via GCS CDN");
    }

    Ok(())
}
