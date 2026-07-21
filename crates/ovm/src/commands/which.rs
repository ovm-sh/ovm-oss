use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::version_manager::VersionManager;
use console::style;

/// Show the path to the active binary for one product (script-friendly).
pub fn run(vm: &VersionManager) -> Result<()> {
    let version = vm.current_version()?.ok_or(OvmError::NoActiveVersion)?;
    let bin = vm.active_binary_path(&version);

    if vm.install_is_complete(&version) {
        println!("{}", bin.display());
    } else {
        println!("{} (binary not found)", bin.display());
    }
    Ok(())
}

/// Show active binary paths for every product (table form).
pub fn run_all() -> Result<()> {
    for product in Product::ALL {
        let label = product.canonical_name();
        match VersionManager::new(product) {
            Ok(vm) => match vm.current_version().ok().flatten() {
                Some(version) => {
                    let bin = vm.active_binary_path(&version);
                    if vm.install_is_complete(&version) {
                        println!("{:<8} {}", label, bin.display());
                    } else {
                        println!(
                            "{:<8} {} {}",
                            label,
                            bin.display(),
                            style("(binary not found)").red()
                        );
                    }
                }
                None => println!("{:<8} {}", label, style("(no active version)").dim()),
            },
            Err(_) => println!("{:<8} {}", label, style("(unavailable)").dim()),
        }
    }
    Ok(())
}
