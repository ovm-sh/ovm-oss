use crate::error::Result;
use crate::version_manager::VersionManager;
use console::style;

use super::format_bytes;

pub fn run(vm: &VersionManager, version: Option<&str>, below: Option<&str>) -> Result<()> {
    if let Some(min) = below {
        let threshold = vm.product().normalize_version(min);
        let (freed, count) = vm.archive_below(&threshold)?;
        println!(
            "{} Archived {} version(s) below {}, freed {}",
            style("✓").green(),
            count,
            threshold,
            format_bytes(freed)
        );
    } else if let Some(v) = version {
        let version = vm.product().normalize_version(v);
        let freed = vm.archive(&version)?;
        println!(
            "{} Archived {}, freed {}",
            style("✓").green(),
            version,
            format_bytes(freed)
        );
    } else {
        eprintln!("Specify a version or use --below <version>");
        eprintln!("  ovm archive {} <version>", vm.product().canonical_name());
        eprintln!(
            "  ovm archive {} --below {}",
            vm.product().canonical_name(),
            match vm.product() {
                crate::product::Product::Claude => "2.0.24",
                crate::product::Product::Codex => "rust-v0.118.0",
                crate::product::Product::Pi => "0.60.0",
            }
        );
    }
    Ok(())
}
