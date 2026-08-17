use crate::error::Result;
use crate::version_manager::VersionManager;
use console::style;

use super::format_bytes;

// What `clean` removes is CACHED DOWNLOAD ARTIFACTS (raw tarballs and
// archives); the installed versions themselves stay untouched. The messages
// say exactly that — "Cleaned all <product> versions, freed 0 B" claimed a
// version removal that never happens, the recurring
// absence-rendered-as-a-verdict shape this repo keeps cataloguing.
pub fn run(vm: &VersionManager, version: Option<&str>, all: bool) -> Result<()> {
    if all || version.is_none() {
        let installed = vm.list_installed()?.len();
        let saved = vm.clean_all()?;
        if saved > 0 {
            println!(
                "{} Cleaned cached download artifacts across {} {} version{}, freed {} (installed versions untouched)",
                style("✓").green(),
                installed,
                vm.product().canonical_name(),
                if installed == 1 { "" } else { "s" },
                format_bytes(saved)
            );
        } else {
            println!(
                "No cached download artifacts to clean across {} {} version{}",
                installed,
                vm.product().canonical_name(),
                if installed == 1 { "" } else { "s" },
            );
        }
    } else if let Some(v) = version {
        let version = vm.product().normalize_version(v);
        let saved = vm.clean(&version)?;
        if saved > 0 {
            println!(
                "{} Cleaned cached download artifacts for {}, freed {} (the installed version is untouched)",
                style("✓").green(),
                version,
                format_bytes(saved)
            );
        } else {
            println!("No cached download artifacts to clean for {}", version);
        }
    }
    Ok(())
}
