use crate::config::{AutoUpdatePolicy, OvmConfig, OvmDirs};
use crate::error::{OvmError, Result};
use crate::product::Product;
use console::style;

const POLICY_HINT: &str = "Unknown auto-update policy. Use `on`, `off`, or `notify`.";
const SELF_SUBJECT: &str = "self";

pub fn run(first: Option<&str>, second: Option<&str>) -> Result<()> {
    let dirs = OvmDirs::new()?;
    let mut config = OvmConfig::load(&dirs.config_file)?;

    match (first, second) {
        (None, None) => print_status(&config),

        // `ovm autoupdate self [on|off|notify]` — OVM's own launch updates.
        (Some(subject), maybe_policy) if subject == SELF_SUBJECT => match maybe_policy {
            None => {
                println!(
                    "OVM self auto-update: {}",
                    style(config.self_.auto_update.label()).green()
                );
            }
            Some(policy) => {
                let policy = parse_policy(policy)?;
                config.self_.auto_update = policy;
                config.save(&dirs.config_file)?;
                println!("OVM self auto-update: {}", style(policy.label()).green());
            }
        },

        // `ovm autoupdate <on|off|notify>` — default for all products.
        (Some(policy), None) if AutoUpdatePolicy::parse(policy).is_some() => {
            let policy = AutoUpdatePolicy::parse(policy).expect("checked above");
            config.auto_update.set_default(policy);
            config.save(&dirs.config_file)?;
            println!("auto-update default: {}", style(policy.label()).green());
        }

        // `ovm autoupdate <product> <on|off|notify>` — one product.
        (Some(product), Some(policy)) => {
            let product = parse_product(product)?;
            let policy = parse_policy(policy)?;
            config.auto_update.set_product(product, policy);
            config.save(&dirs.config_file)?;
            println!(
                "{} auto-update: {}",
                product.display_name(),
                style(policy.label()).green()
            );
        }

        // `ovm autoupdate <product>` — show one product's policy.
        (Some(product), None) if Product::parse(product).is_some() => {
            let product = Product::parse(product).expect("checked above");
            println!(
                "{} auto-update: {}",
                product.display_name(),
                style(config.auto_update.policy_for(product).label()).green()
            );
        }

        _ => {
            return Err(OvmError::Message(
                "Usage: ovm autoupdate [on|off|notify], ovm autoupdate <product> [on|off|notify], \
                 or ovm autoupdate self [on|off|notify]"
                    .into(),
            ));
        }
    }

    Ok(())
}

fn parse_policy(value: &str) -> Result<AutoUpdatePolicy> {
    AutoUpdatePolicy::parse(value).ok_or_else(|| OvmError::Message(POLICY_HINT.into()))
}

fn parse_product(value: &str) -> Result<Product> {
    Product::parse(value).ok_or_else(|| {
        OvmError::Message("Unknown product. Use one of: claude, cc, codex, cx, pi.".into())
    })
}

fn print_status(config: &OvmConfig) {
    let default = status_lines(config);
    println!(
        "auto-update default: {}",
        style(config.auto_update.default.label()).green()
    );
    for (subject, policy) in default {
        println!("  {subject:<6} {}", style(policy).green());
    }
}

/// The per-subject policy rows shown under the default, as plain
/// `(subject, policy-label)` pairs. Self is always listed alongside the
/// products so `ovm autoupdate` surfaces OVM's own policy.
fn status_lines(config: &OvmConfig) -> Vec<(&'static str, &'static str)> {
    let mut rows: Vec<(&'static str, &'static str)> = Product::ALL
        .iter()
        .map(|product| {
            (
                product.canonical_name(),
                config.auto_update.policy_for(*product).label(),
            )
        })
        .collect();
    rows.push(("self", config.self_.auto_update.label()));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_parser_accepts_off_on_and_notify() {
        assert_eq!(AutoUpdatePolicy::parse("off"), Some(AutoUpdatePolicy::Off));
        assert_eq!(AutoUpdatePolicy::parse("on"), Some(AutoUpdatePolicy::On));
        assert_eq!(
            AutoUpdatePolicy::parse("notify"),
            Some(AutoUpdatePolicy::Notify)
        );
        assert_eq!(AutoUpdatePolicy::parse("other"), None);
    }

    #[test]
    fn status_lines_include_self_and_products() {
        let mut config = OvmConfig::default();
        config.self_.auto_update = AutoUpdatePolicy::Notify;
        config
            .auto_update
            .set_product(Product::Codex, AutoUpdatePolicy::Off);
        let rows = status_lines(&config);

        for product in Product::ALL {
            assert!(
                rows.iter()
                    .any(|(name, _)| *name == product.canonical_name()),
                "missing {}",
                product.canonical_name()
            );
        }
        let self_row = rows.iter().find(|(name, _)| *name == "self");
        assert_eq!(self_row.map(|(_, policy)| *policy), Some("notify"));
        let codex_row = rows.iter().find(|(name, _)| *name == "codex");
        assert_eq!(codex_row.map(|(_, policy)| *policy), Some("off"));
    }
}
