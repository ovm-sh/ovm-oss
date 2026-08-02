//! `ovm update` — the imperative "bring my tools up to date, now".
//!
//! Two shapes share one verb:
//!   - `ovm update [product]` *does* the update immediately;
//!   - `ovm update auto …` configures the launch-time policy that used to be
//!     the whole of `ovm autoupdate` (still accepted as a hidden alias).
//!
//! The imperative form deliberately reuses the launch-time machinery
//! ([`super::launch::install_and_use_latest`]) so an explicit update and an
//! `autoUpdate: on` launch converge on exactly the same on-disk state. The one
//! difference is where "latest" comes from: a launch reads the local cache and
//! never blocks on the network, while this command was *asked* for by name and
//! so resolves upstream the same way `ovm <product> latest` does.

use crate::config::AutoUpdatePolicy;
use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::version_manager::VersionManager;
use console::style;

/// `ovm update auto …` — the settings sub-verb.
const AUTO_SUBCOMMAND: &str = "auto";
/// `self` addresses OVM itself rather than a managed product.
const SELF_SUBJECT: &str = "self";

pub fn run(first: Option<&str>, second: Option<&str>, third: Option<&str>) -> Result<()> {
    match first {
        // `ovm update auto [product|self] [on|off|notify]` — the setting.
        Some(AUTO_SUBCOMMAND) => super::autoupdate::run(second, third),

        // `ovm update self` — OVM's own imperative update, so the verb covers
        // every subject `ovm update auto` can name.
        Some(SELF_SUBJECT) => {
            reject_trailing(second, third, "ovm update self")?;
            super::self_update::run(None, "auto", false)
        }

        // `ovm update on` reads as a setting; say so instead of guessing.
        Some(value) if AutoUpdatePolicy::parse(value).is_some() => Err(OvmError::Message(format!(
            "`{value}` is an auto-update setting, not a product.\n\
             Set it with `ovm update auto {value}`, or run `ovm update` to update everything now."
        ))),

        Some(value) => {
            let product = Product::parse(value).ok_or_else(|| {
                OvmError::Message(format!(
                    "Unknown product {value}. Use one of: claude, cc, codex, cx, pi, self — \
                     or `ovm update auto` for the launch-time setting."
                ))
            })?;
            // `ovm update codex notify` is the old setting shape under the new
            // verb. Updating Codex right now is not what that user asked for.
            if let Some(policy) = second {
                if AutoUpdatePolicy::parse(policy).is_some() {
                    return Err(OvmError::Message(format!(
                        "`{policy}` is an auto-update setting. Did you mean `ovm update auto {value} {policy}`?"
                    )));
                }
            }
            reject_trailing(second, third, "ovm update <product>")?;
            update_now(&[product], Scope::Named)
        }

        None => update_now(&Product::ALL, Scope::All),
    }
}

fn reject_trailing(second: Option<&str>, third: Option<&str>, usage: &str) -> Result<()> {
    match second.or(third) {
        Some(extra) => Err(OvmError::Message(format!(
            "Unexpected argument `{extra}`. Usage: {usage}"
        ))),
        None => Ok(()),
    }
}

/// Whether the user named this product or swept every product at once. The
/// distinction only matters for pins: see [`plan_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// `ovm update` — every managed product.
    All,
    /// `ovm update <product>` — this one, by name.
    Named,
}

/// What to do about one product, decided before any network or disk work.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    /// Nothing is managed for this product yet — there is no "latest" to move
    /// to, and an update is not the command that should start downloading a
    /// product the user never installed.
    NotInstalled,
    /// A local `dev:` build. OVM has no upstream to compare it against, so the
    /// launch path leaves it alone and so does this.
    DevBuild(String),
    /// Deliberately pinned and not named on the command line. A sweep must not
    /// quietly undo a pin the user set on purpose.
    PinnedSkip(String),
    /// Resolve the latest release and move to it if it is newer.
    Resolve(String),
}

/// The pure decision. `baseline` is the version an update would move away from
/// (the active version, or the newest installed one when nothing is selected).
fn plan_for(baseline: Option<&str>, pin: Option<&str>, scope: Scope) -> Plan {
    let Some(baseline) = baseline else {
        return Plan::NotInstalled;
    };
    if baseline.starts_with("dev:") {
        return Plan::DevBuild(baseline.to_string());
    }
    // A pin is an explicit "keep me here". Naming the product overrides it
    // (and clears it); a bare `ovm update` reports it and moves on.
    if scope == Scope::All && pin == Some(baseline) {
        return Plan::PinnedSkip(baseline.to_string());
    }
    Plan::Resolve(baseline.to_string())
}

/// What actually happened to one product.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Updated { from: String, to: String },
    AlreadyLatest(String),
    Skipped { detail: String, hint: String },
    Failed(String),
}

impl Outcome {
    fn is_failure(&self) -> bool {
        matches!(self, Outcome::Failed(_))
    }
}

fn update_now(products: &[Product], scope: Scope) -> Result<()> {
    crate::mochi::say(
        crate::mochi::WORKING,
        &match products {
            [only] => format!("Updating {} to the latest release", only.display_name()),
            _ => "Updating installed products to the latest release".to_string(),
        },
    );
    println!();

    let results: Vec<(Product, Outcome)> = products
        .iter()
        .map(|product| (*product, update_one(*product, scope)))
        .collect();

    for (product, outcome) in &results {
        print_row(*product, outcome);
    }
    println!();
    print_summary(&results);

    let failed: Vec<&'static str> = results
        .iter()
        .filter(|(_, outcome)| outcome.is_failure())
        .map(|(product, _)| product.display_name())
        .collect();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(OvmError::Message(format!(
            "Could not update: {}. Nothing else was changed.",
            failed.join(", ")
        )))
    }
}

fn update_one(product: Product, scope: Scope) -> Outcome {
    let vm = match VersionManager::new(product) {
        Ok(vm) => vm,
        Err(error) => return Outcome::Failed(error.to_string()),
    };

    // Nothing selected but something installed still counts as installed: the
    // update selects the newest as it goes, exactly like a first launch would.
    let baseline = match vm.current_version() {
        Ok(Some(version)) => Some(version),
        Ok(None) => vm
            .list_installed()
            .ok()
            .and_then(|v| v.into_iter().next_back()),
        Err(error) => return Outcome::Failed(error.to_string()),
    };
    let pin = vm.read_pin();

    match plan_for(baseline.as_deref(), pin.as_deref(), scope) {
        Plan::NotInstalled => Outcome::Skipped {
            detail: "not installed".to_string(),
            hint: format!("ovm install {} latest", product.canonical_name()),
        },
        Plan::DevBuild(version) => Outcome::Skipped {
            detail: format!("{version} (local dev build)"),
            hint: format!("ovm use {} <version>", product.canonical_name()),
        },
        Plan::PinnedSkip(version) => Outcome::Skipped {
            detail: format!("pinned to {version}"),
            hint: format!("ovm update {}", product.canonical_name()),
        },
        Plan::Resolve(baseline) => resolve_and_apply(&vm, &baseline),
    }
}

/// Resolve upstream latest and, when it is newer, install + activate it.
///
/// The resolver falls back to the newest *installed* release when the update
/// service is unreachable, printing its own "could not reach update service"
/// warning first — so an offline `ovm update` reports "already latest" only
/// directly underneath that warning, never in place of it.
fn resolve_and_apply(vm: &VersionManager, baseline: &str) -> Outcome {
    let product = vm.product();
    let latest = match vm.latest_available_version() {
        Ok(latest) => product.normalize_version(&latest),
        Err(error) => return Outcome::Failed(error.to_string()),
    };

    if !product.is_newer(&latest, baseline) {
        return Outcome::AlreadyLatest(baseline.to_string());
    }

    match super::launch::install_and_use_latest(vm, &latest) {
        Ok(version) => {
            // Same post-switch hygiene `ovm use` performs, minus its banner:
            // keep Claude's launcher owned and let companions warn about a
            // state-DB migration before the next launch runs into it.
            super::maintain_claude_launcher(vm);
            crate::companions::run(
                &vm.dirs,
                product,
                crate::companions::Event::PostSwitch,
                &version,
                &vm.active_binary_path(&version),
            );
            Outcome::Updated {
                from: baseline.to_string(),
                to: version,
            }
        }
        Err(error) => Outcome::Failed(error.to_string()),
    }
}

fn print_row(product: Product, outcome: &Outcome) {
    let name = format!("{:<12}", product.display_name());
    match outcome {
        Outcome::Updated { from, to } => println!(
            "  {name} {} {} {}",
            style(from).dim(),
            style("→").cyan(),
            style(to).green().bold()
        ),
        Outcome::AlreadyLatest(version) => {
            println!("  {name} {}  {}", version, style("already latest").dim())
        }
        Outcome::Skipped { detail, hint } => println!(
            "  {name} {}  {}",
            style(detail).dim(),
            style(format!("({hint})")).dim()
        ),
        Outcome::Failed(error) => println!(
            "  {name} {}  {}",
            style("failed").red().bold(),
            style(error).dim()
        ),
    }
}

fn print_summary(results: &[(Product, Outcome)]) {
    println!("  {}", style(summary_line(results)).dim());
}

/// The closing tally. Deliberately counts *what happened* rather than claiming
/// a clean bill of health: "3 already latest" and "3 skipped" must never render
/// as the same sentence.
fn summary_line(results: &[(Product, Outcome)]) -> String {
    let mut updated = 0;
    let mut current = 0;
    let mut skipped = 0;
    let mut failed = 0;
    for (_, outcome) in results {
        match outcome {
            Outcome::Updated { .. } => updated += 1,
            Outcome::AlreadyLatest(_) => current += 1,
            Outcome::Skipped { .. } => skipped += 1,
            Outcome::Failed(_) => failed += 1,
        }
    }

    let mut parts = vec![format!("{updated} updated")];
    if current > 0 {
        parts.push(format!("{current} already latest"));
    }
    if skipped > 0 {
        parts.push(format!("{skipped} skipped"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_installed_is_reported_not_updated() {
        assert_eq!(plan_for(None, None, Scope::All), Plan::NotInstalled);
        assert_eq!(plan_for(None, None, Scope::Named), Plan::NotInstalled);
    }

    #[test]
    fn dev_builds_are_left_alone_like_a_launch_leaves_them() {
        assert_eq!(
            plan_for(Some("dev:resume-fix"), None, Scope::Named),
            Plan::DevBuild("dev:resume-fix".into())
        );
    }

    #[test]
    fn a_sweep_respects_a_pin_but_naming_the_product_overrides_it() {
        // `ovm update` must not silently undo a deliberate `ovm use`.
        assert_eq!(
            plan_for(Some("2.1.159"), Some("2.1.159"), Scope::All),
            Plan::PinnedSkip("2.1.159".into())
        );
        // `ovm update claude` is unambiguous intent — move it.
        assert_eq!(
            plan_for(Some("2.1.159"), Some("2.1.159"), Scope::Named),
            Plan::Resolve("2.1.159".into())
        );
    }

    #[test]
    fn a_stale_pin_for_another_version_does_not_block_a_sweep() {
        // The pin names a version that is no longer active — it says nothing
        // about the version we would be moving away from.
        assert_eq!(
            plan_for(Some("2.1.170"), Some("2.1.159"), Scope::All),
            Plan::Resolve("2.1.170".into())
        );
    }

    #[test]
    fn summary_distinguishes_up_to_date_from_skipped() {
        let latest = vec![(Product::Claude, Outcome::AlreadyLatest("2.1.170".into()))];
        let skipped = vec![(
            Product::Claude,
            Outcome::Skipped {
                detail: "not installed".into(),
                hint: "ovm install claude latest".into(),
            },
        )];

        assert_eq!(summary_line(&latest), "0 updated, 1 already latest");
        assert_eq!(summary_line(&skipped), "0 updated, 1 skipped");
    }

    #[test]
    fn summary_counts_every_outcome() {
        let results = vec![
            (
                Product::Claude,
                Outcome::Updated {
                    from: "2.1.159".into(),
                    to: "2.1.170".into(),
                },
            ),
            (
                Product::Codex,
                Outcome::AlreadyLatest("rust-v0.146.0".into()),
            ),
            (Product::Pi, Outcome::Failed("offline".into())),
        ];

        assert_eq!(
            summary_line(&results),
            "1 updated, 1 already latest, 1 failed"
        );
    }

    #[test]
    fn policy_words_are_rejected_as_products() {
        for policy in ["on", "off", "notify"] {
            let error = run(Some(policy), None, None).expect_err("policy is not a product");
            assert!(
                error.to_string().contains("ovm update auto"),
                "should point at the setting form: {error}"
            );
        }
    }

    #[test]
    fn the_old_setting_shape_under_the_new_verb_is_redirected() {
        let error = run(Some("codex"), Some("notify"), None).expect_err("setting shape");
        assert!(
            error.to_string().contains("ovm update auto codex notify"),
            "should suggest the auto form: {error}"
        );
    }

    #[test]
    fn unknown_products_name_the_valid_subjects() {
        let error = run(Some("nope"), None, None).expect_err("unknown product");
        let text = error.to_string();
        assert!(
            text.contains("claude") && text.contains("self") && text.contains("ovm update auto")
        );
    }
}
