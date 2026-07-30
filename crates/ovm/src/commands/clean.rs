use crate::error::Result;
use crate::version_manager::VersionManager;
use console::style;

use super::format_bytes;

pub fn run(vm: &VersionManager, version: Option<&str>, all: bool) -> Result<()> {
    if all || version.is_none() {
        let saved = vm.clean_all()?;
        println!(
            "{} Cleaned all {} versions, freed {}",
            style("✓").green(),
            vm.product().canonical_name(),
            format_bytes(saved)
        );
    } else if let Some(v) = version {
        let version = vm.product().normalize_version(v);
        let saved = vm.clean(&version)?;
        if saved > 0 {
            println!(
                "{} Cleaned {}, freed {}",
                style("✓").green(),
                version,
                format_bytes(saved)
            );
        } else {
            println!("Nothing to clean for {}", version);
        }
    }
    Ok(())
}
