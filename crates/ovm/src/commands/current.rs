use crate::error::Result;
use crate::product::Product;
use crate::version_manager::VersionManager;
use console::style;

/// Show the active version for one product (script-friendly, just prints the version).
pub fn run(vm: &VersionManager) -> Result<()> {
    match vm.current_version()? {
        Some(version) => println!("{version}"),
        None => {
            eprintln!(
                "{}",
                style(format!(
                    "No active {} version",
                    vm.product().canonical_name()
                ))
                .dim()
            );
            std::process::exit(1);
        }
    }
    Ok(())
}

/// Show active versions for all products + OVM itself (status dashboard).
pub fn run_all() -> Result<()> {
    let ovm_version = env!("CARGO_PKG_VERSION");
    println!("{:<8} {}", "ovm", style(ovm_version).dim());

    for product in Product::ALL {
        match VersionManager::new(product) {
            Ok(vm) => {
                let active = vm.current_version().ok().flatten();
                match active {
                    Some(v) => println!(
                        "{:<8} {}",
                        product.canonical_name(),
                        style(v).green().bold()
                    ),
                    None => println!(
                        "{:<8} {}",
                        product.canonical_name(),
                        style("(not installed)").dim()
                    ),
                }
            }
            Err(_) => {
                println!(
                    "{:<8} {}",
                    product.canonical_name(),
                    style("(unavailable)").dim()
                );
            }
        }
    }
    Ok(())
}
