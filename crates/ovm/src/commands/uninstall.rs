use crate::error::Result;
use crate::version_manager::VersionManager;
use console::style;

pub fn run(vm: &VersionManager, version: &str) -> Result<()> {
    vm.uninstall(version)?;
    println!(
        "{} Uninstalled {} {}",
        style("✓").green(),
        vm.product().display_name(),
        vm.product().normalize_version(version)
    );
    Ok(())
}
