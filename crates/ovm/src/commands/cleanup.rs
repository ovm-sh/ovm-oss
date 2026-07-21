use crate::config::{CleanupRetention, OvmConfig, OvmDirs};
use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::version_manager::VersionManager;
use console::style;

use super::format_bytes;

pub fn run(retention: Option<&str>) -> Result<()> {
    let dirs = OvmDirs::new()?;
    let mut config = OvmConfig::load(&dirs.config_file)?;

    match retention {
        None => {
            println!(
                "cleanup retention: {}",
                style(config.cleanup.retention.label()).green()
            );
        }
        Some(value) => {
            let retention = CleanupRetention::parse(value).ok_or_else(|| {
                OvmError::Message("Unknown cleanup retention. Use `30`, `60`, or `never`.".into())
            })?;
            config.cleanup.retention = retention;
            config.save(&dirs.config_file)?;
            println!("cleanup retention: {}", style(retention.label()).green());
        }
    }

    Ok(())
}

pub(crate) fn prune_all_products(config: &OvmConfig) {
    let Some(days) = config.cleanup.retention.days() else {
        return;
    };

    let mut total_count = 0usize;
    let mut total_freed = 0u64;

    for product in Product::ALL {
        let Ok(vm) = VersionManager::new(product) else {
            continue;
        };
        match vm.prune_inactive_installs_older_than(days) {
            Ok((freed, count)) => {
                total_count += count;
                total_freed += freed;
            }
            Err(error) => {
                if std::env::var_os("OVM_VERBOSE").is_some() {
                    eprintln!(
                        "  {} cleanup skipped for {}: {}",
                        style("·").dim(),
                        product.display_name(),
                        error
                    );
                }
            }
        }
    }

    if total_count > 0 {
        eprintln!(
            "  {} Cleaned up {} old install(s), freed {}",
            style("✓").green(),
            total_count,
            format_bytes(total_freed)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_parser_accepts_supported_values() {
        assert_eq!(
            CleanupRetention::parse("30"),
            Some(CleanupRetention::Days30)
        );
        assert_eq!(
            CleanupRetention::parse("60"),
            Some(CleanupRetention::Days60)
        );
        assert_eq!(
            CleanupRetention::parse("never"),
            Some(CleanupRetention::Never)
        );
        assert_eq!(CleanupRetention::parse("90"), None);
    }
}
