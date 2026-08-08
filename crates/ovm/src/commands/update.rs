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

use crate::config::{AutoUpdatePolicy, OvmConfig, OvmDirs};
use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::version_manager::VersionManager;
use console::style;

/// `ovm update auto …` — the settings sub-verb.
const AUTO_SUBCOMMAND: &str = "auto";
/// `self` addresses OVM itself rather than a managed product.
const SELF_SUBJECT: &str = "self";

pub fn run(
    first: Option<&str>,
    second: Option<&str>,
    third: Option<&str>,
    yes: bool,
    check_only: bool,
) -> Result<()> {
    let mode = Mode::resolve(yes, check_only);
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
                    "Unknown product {value}. Use one of: {}, self — \
                     or `ovm update auto` for the launch-time setting.",
                    Product::accepted_names()
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
            update_now(&[product], Scope::Named, mode)
        }

        None => update_now(&Product::ALL, Scope::All, mode),
    }
}

/// How the command decides *what* to update once it knows what it could.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Show what would change and touch nothing (`--check`).
    CheckOnly,
    /// Take everything found: `--yes`, or no terminal to ask on. A script that
    /// piped `ovm update` yesterday must not start waiting on a prompt today.
    All,
    /// Ask which ones (a terminal, no flags).
    Ask,
}

impl Mode {
    fn resolve(yes: bool, check_only: bool) -> Self {
        if check_only {
            Mode::CheckOnly
        } else if yes || !interactive() {
            Mode::All
        } else {
            Mode::Ask
        }
    }
}

/// Asking requires both halves of a conversation: a terminal to draw the
/// picker on and one to read the keys from.
fn interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
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
    /// Deliberately pinned and not named on the command line, with nobody
    /// there to ask. A hands-off sweep must not quietly undo a pin the user
    /// set on purpose.
    PinnedSkip(String),
    /// Resolve the latest release and move to it if it is newer. `pinned`
    /// carries the pin into the offer: the picker shows the row unticked, so
    /// moving the pin takes a deliberate keypress instead of happening as a
    /// side effect of "yes, all of them".
    Resolve { baseline: String, pinned: bool },
}

/// The pure decision. `baseline` is the version an update would move away from
/// (the active version, or the newest installed one when nothing is selected).
/// `offer_pinned` is whether a pin may be surfaced as an opt-in choice rather
/// than skipped outright — true exactly when a human will see the result
/// before anything installs (the picker, `--check`), false for `--yes` and
/// scripted sweeps.
fn plan_for(baseline: Option<&str>, pin: Option<&str>, scope: Scope, offer_pinned: bool) -> Plan {
    let Some(baseline) = baseline else {
        return Plan::NotInstalled;
    };
    if baseline.starts_with("dev:") {
        return Plan::DevBuild(baseline.to_string());
    }
    // A pin is an explicit "keep me here". Naming the product overrides it
    // (and clears it); a bare `ovm update` offers it when it can ask and
    // reports it when it cannot.
    if scope == Scope::All && pin == Some(baseline) {
        if offer_pinned {
            return Plan::Resolve {
                baseline: baseline.to_string(),
                pinned: true,
            };
        }
        return Plan::PinnedSkip(baseline.to_string());
    }
    Plan::Resolve {
        baseline: baseline.to_string(),
        pinned: false,
    }
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

fn update_now(products: &[Product], scope: Scope, mode: Mode) -> Result<()> {
    crate::mochi::say(
        crate::mochi::WORKING,
        &match (products, mode) {
            ([only], Mode::CheckOnly) => format!("Checking {} for updates", only.display_name()),
            ([only], _) => format!("Updating {} to the latest release", only.display_name()),
            (_, Mode::CheckOnly) => "Checking installed products for updates".to_string(),
            _ => "Checking installed products for updates".to_string(),
        },
    );
    println!();

    // Pins become opt-in rows only where someone will see them before
    // anything installs; a `--yes` or scripted sweep keeps the hands-off rule.
    let offer_pinned = mode != Mode::All;

    // Phase 1 — look, decide nothing. Resolving every product before touching
    // any of them is what makes the choice possible: you cannot pick from a
    // list that is being installed as it is printed.
    let checks: Vec<(Product, Check)> = products
        .iter()
        .map(|product| (*product, check_one(*product, scope, offer_pinned)))
        .collect();

    // Everything that will not change, said once and up front.
    for (product, check) in &checks {
        if let Some(outcome) = check.as_settled_outcome() {
            print_row(*product, &outcome);
        }
    }

    let available: Vec<(Product, &Available)> = checks
        .iter()
        .filter_map(|(product, check)| match check {
            Check::Available(available) => Some((*product, available)),
            _ => None,
        })
        .collect();

    // The bare interactive sweep doubles as the settings screen: updates on
    // top, launch-time auto-update policies underneath, one enter for both.
    // A named product stays a plain "update this" question. The settings are
    // an extra offer, so a config that will not load costs the offer, never
    // the update — `ovm update auto` will surface the same error with a
    // stack that actually points at the setting.
    let mut settings = if mode == Mode::Ask && scope == Scope::All {
        match Settings::load() {
            Ok(settings) => Some(settings),
            Err(error) => {
                println!(
                    "  {}",
                    style(format!("auto-update settings unavailable: {error}")).dim()
                );
                None
            }
        }
    } else {
        None
    };

    // Failures already happened during the checks; whichever way the
    // interaction below ends, they must still decide the exit code.
    let settled: Vec<(Product, Outcome)> = checks
        .iter()
        .filter_map(|(p, c)| c.as_settled_outcome().map(|o| (*p, o)))
        .collect();

    if available.is_empty() && settings.is_none() {
        println!();
        print_summary(&settled);
        return report_failures(&settled, false);
    }

    // Phase 2 — choose. `--check` stops here by definition.
    if mode == Mode::CheckOnly {
        println!();
        println!("  {}", style("available").bold());
        for (product, update) in &available {
            print_available(*product, update);
        }
        println!();
        let pinned_count = available.iter().filter(|(_, u)| u.pinned).count();
        let yes_hint = if pinned_count > 0 {
            "`ovm update --yes` for all but pinned"
        } else {
            "`ovm update --yes` for all"
        };
        println!(
            "  {}",
            style(format!(
                "{} to install — run `ovm update` to choose, or {yes_hint}",
                available.len()
            ))
            .dim()
        );
        return Ok(());
    }

    let (chosen, chosen_policies): (Vec<usize>, Vec<AutoUpdatePolicy>) = match mode {
        Mode::All => ((0..available.len()).collect(), Vec::new()),
        Mode::Ask => {
            let names = settings.as_ref().map(Settings::names).unwrap_or_default();
            let initial = settings
                .as_ref()
                .map(Settings::policies)
                .unwrap_or_default();
            match super::update_picker::choose(&available, &names, initial)? {
                Some(choice) => choice,
                // Cancelling is a valid answer, not an error: nothing was
                // touched — not the products, not the settings. Check
                // failures still count, though; declining to install does
                // not un-fail a product that could not even be looked at.
                None => {
                    println!();
                    println!("  {}", style("Nothing updated.").dim());
                    return report_failures(&settled, false);
                }
            }
        }
        Mode::CheckOnly => unreachable!("check-only returns above"),
    };

    // Settings save first: a policy edit must stick even if a download after
    // it fails halfway.
    let setting_changes = match settings.as_mut() {
        Some(settings) => settings.apply(&chosen_policies)?,
        None => Vec::new(),
    };

    if chosen.is_empty() && setting_changes.is_empty() {
        println!();
        println!("  {}", style("Nothing selected — nothing updated.").dim());
        return report_failures(&settled, false);
    }

    // Phase 3 — apply only what was chosen.
    println!();
    let mut results: Vec<(Product, Outcome)> = settled.clone();
    for index in chosen {
        let (product, update) = available[index];
        let outcome = apply(product, update);
        print_row(product, &outcome);
        results.push((product, outcome));
    }
    for change in &setting_changes {
        print_setting_change(change);
    }

    println!();
    print_summary(&results);
    report_failures(&results, !setting_changes.is_empty())
}

/// The launch-time auto-update policies as one editable block: the three
/// products in `Product::ALL` order, then OVM itself. Names, values, and the
/// save all speak the same row order, so the picker can stay a plain list.
struct Settings {
    dirs: OvmDirs,
    config: OvmConfig,
}

/// The settings row for OVM's own launch updates.
const SELF_ROW: &str = "OVM";

/// One policy row the picker moved, kept for printing after the save.
struct SettingChange {
    name: &'static str,
    from: AutoUpdatePolicy,
    to: AutoUpdatePolicy,
}

impl Settings {
    fn load() -> Result<Self> {
        let dirs = OvmDirs::new()?;
        let config = OvmConfig::load(&dirs.config_file)?;
        Ok(Self { dirs, config })
    }

    fn names(&self) -> Vec<&'static str> {
        Product::ALL
            .iter()
            .map(|product| product.display_name())
            .chain([SELF_ROW])
            .collect()
    }

    fn policies(&self) -> Vec<AutoUpdatePolicy> {
        Product::ALL
            .iter()
            .map(|product| self.config.auto_update.policy_for(*product))
            .chain([self.config.self_.auto_update])
            .collect()
    }

    /// Persist the rows the picker moved, if any, and say which they were.
    /// Rows that came back unchanged write nothing — confirming the screen
    /// without touching it must not start pinning per-product overrides.
    fn apply(&mut self, chosen: &[AutoUpdatePolicy]) -> Result<Vec<SettingChange>> {
        let before = self.policies();
        let names = self.names();
        let mut changes = Vec::new();
        for (index, (&from, &to)) in before.iter().zip(chosen).enumerate() {
            if from == to {
                continue;
            }
            match Product::ALL.get(index) {
                Some(product) => self.config.set_auto_update_product(*product, to),
                None => self.config.set_self_auto_update(to),
            }
            changes.push(SettingChange {
                name: names[index],
                from,
                to,
            });
        }
        if !changes.is_empty() {
            self.config.save(&self.dirs.config_file)?;
        }
        Ok(changes)
    }
}

fn print_setting_change(change: &SettingChange) {
    println!(
        "  {:<12} {} {} {} {}",
        change.name,
        style("auto-update").dim(),
        style(change.from.label()).dim(),
        style("→").cyan(),
        style(change.to.label()).green().bold()
    );
}

/// The failing exit, worded to match what actually happened. A failure next to
/// successful work is a *partial* run: claiming "nothing else was changed" when
/// a product was just installed contradicts the summary printed one line above
/// it, and would send someone looking for an update they already have.
fn report_failures(results: &[(Product, Outcome)], settings_changed: bool) -> Result<()> {
    let failed: Vec<&'static str> = results
        .iter()
        .filter(|(_, outcome)| outcome.is_failure())
        .map(|(product, _)| product.display_name())
        .collect();
    if failed.is_empty() {
        return Ok(());
    }
    let changed = settings_changed
        || results
            .iter()
            .any(|(_, outcome)| matches!(outcome, Outcome::Updated { .. }));
    let tail = if changed {
        "Everything else was applied as shown above."
    } else {
        "Nothing else was changed."
    };
    Err(OvmError::Message(format!(
        "Could not update: {}. {tail}",
        failed.join(", ")
    )))
}

/// An update that exists and has not been applied. `pinned` means the current
/// version is a deliberate pin, so the picker offers this row unticked and
/// taking it is a conscious "move my pin".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Available {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) pinned: bool,
}

/// The result of *looking* at one product. Separate from [`Outcome`] because
/// "an update exists" is not yet something that happened.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Check {
    Available(Available),
    AlreadyLatest(String),
    Skipped { detail: String, hint: String },
    Failed(String),
}

impl Check {
    /// Everything except an available update is already a final outcome.
    fn as_settled_outcome(&self) -> Option<Outcome> {
        match self {
            Check::Available(_) => None,
            Check::AlreadyLatest(version) => Some(Outcome::AlreadyLatest(version.clone())),
            Check::Skipped { detail, hint } => Some(Outcome::Skipped {
                detail: detail.clone(),
                hint: hint.clone(),
            }),
            Check::Failed(error) => Some(Outcome::Failed(error.clone())),
        }
    }
}

fn check_one(product: Product, scope: Scope, offer_pinned: bool) -> Check {
    let vm = match VersionManager::new(product) {
        Ok(vm) => vm,
        Err(error) => return Check::Failed(error.to_string()),
    };

    // Nothing selected but something installed still counts as installed: the
    // update selects the newest as it goes, exactly like a first launch would.
    let baseline = match vm.current_version() {
        Ok(Some(version)) => Some(version),
        Ok(None) => vm
            .list_installed()
            .ok()
            .and_then(|v| v.into_iter().next_back()),
        Err(error) => return Check::Failed(error.to_string()),
    };
    let pin = vm.read_pin();

    match plan_for(baseline.as_deref(), pin.as_deref(), scope, offer_pinned) {
        Plan::NotInstalled => Check::Skipped {
            detail: "not installed".to_string(),
            hint: format!("ovm install {} latest", product.canonical_name()),
        },
        Plan::DevBuild(version) => Check::Skipped {
            detail: format!("{version} (local dev build)"),
            hint: format!("ovm use {} <version>", product.canonical_name()),
        },
        Plan::PinnedSkip(version) => pinned_skip(product, &version),
        Plan::Resolve { baseline, pinned } => {
            let check = resolve(&vm, &baseline, pinned);
            // A pin with nothing newer — or whose lookup failed — is simply a
            // pin. The offer was optional; its absence must not turn into an
            // "already latest" claim or fail the sweep.
            if pinned && !matches!(check, Check::Available(_)) {
                return pinned_skip(product, &baseline);
            }
            check
        }
    }
}

fn pinned_skip(product: Product, version: &str) -> Check {
    Check::Skipped {
        detail: format!("pinned to {version}"),
        hint: format!("ovm update {}", product.canonical_name()),
    }
}

/// Resolve upstream latest and report whether it is newer. Installs nothing.
///
/// The resolver falls back to the newest *installed* release when the update
/// service is unreachable, printing its own "could not reach update service"
/// warning first — so an offline check reports "already latest" only directly
/// underneath that warning, never in place of it.
fn resolve(vm: &VersionManager, baseline: &str, pinned: bool) -> Check {
    let product = vm.product();
    let latest = match vm.latest_available_version() {
        Ok(latest) => product.normalize_version(&latest),
        Err(error) => return Check::Failed(error.to_string()),
    };

    if !product.is_newer(&latest, baseline) {
        return Check::AlreadyLatest(baseline.to_string());
    }

    Check::Available(Available {
        from: baseline.to_string(),
        to: latest,
        pinned,
    })
}

/// Install and activate one already-resolved update.
fn apply(product: Product, update: &Available) -> Outcome {
    let vm = match VersionManager::new(product) {
        Ok(vm) => vm,
        Err(error) => return Outcome::Failed(error.to_string()),
    };
    let vm = &vm;
    let baseline = update.from.as_str();

    match super::launch::install_and_use_latest(vm, &update.to) {
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

/// A row for an update that exists but has not been applied.
fn print_available(product: Product, update: &Available) {
    let pin_note = if update.pinned {
        format!("  {}", style("(pinned)").dim())
    } else {
        String::new()
    };
    println!(
        "  {:<12} {} {} {}{pin_note}",
        product.display_name(),
        style(&update.from).dim(),
        style("→").cyan(),
        style(&update.to).green().bold()
    );
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

    fn resolve_plan(baseline: &str, pinned: bool) -> Plan {
        Plan::Resolve {
            baseline: baseline.into(),
            pinned,
        }
    }

    #[test]
    fn nothing_installed_is_reported_not_updated() {
        assert_eq!(plan_for(None, None, Scope::All, true), Plan::NotInstalled);
        assert_eq!(
            plan_for(None, None, Scope::Named, false),
            Plan::NotInstalled
        );
    }

    #[test]
    fn dev_builds_are_left_alone_like_a_launch_leaves_them() {
        assert_eq!(
            plan_for(Some("dev:resume-fix"), None, Scope::Named, true),
            Plan::DevBuild("dev:resume-fix".into())
        );
    }

    #[test]
    fn a_hands_off_sweep_respects_a_pin_but_naming_the_product_overrides_it() {
        // `ovm update --yes` (or a script) must not silently undo a
        // deliberate `ovm use`.
        assert_eq!(
            plan_for(Some("2.1.159"), Some("2.1.159"), Scope::All, false),
            Plan::PinnedSkip("2.1.159".into())
        );
        // `ovm update claude` is unambiguous intent — move it.
        assert_eq!(
            plan_for(Some("2.1.159"), Some("2.1.159"), Scope::Named, false),
            resolve_plan("2.1.159", false)
        );
    }

    #[test]
    fn an_asking_sweep_offers_the_pin_instead_of_skipping_it() {
        // With a picker (or `--check`) in front of the user, the pin becomes
        // an unticked row rather than an invisible skip: ticking it is the
        // explicit consent the skip existed to require.
        assert_eq!(
            plan_for(Some("2.1.220"), Some("2.1.220"), Scope::All, true),
            resolve_plan("2.1.220", true)
        );
    }

    #[test]
    fn a_stale_pin_for_another_version_does_not_block_a_sweep() {
        // The pin names a version that is no longer active — it says nothing
        // about the version we would be moving away from.
        assert_eq!(
            plan_for(Some("2.1.170"), Some("2.1.159"), Scope::All, false),
            resolve_plan("2.1.170", false)
        );
        assert_eq!(
            plan_for(Some("2.1.170"), Some("2.1.159"), Scope::All, true),
            resolve_plan("2.1.170", false)
        );
    }

    #[test]
    fn settings_rows_cover_every_product_and_then_ovm_itself() {
        let dirs = OvmDirs::at(tempfile::tempdir().unwrap().path().join(".ovm"));
        let settings = Settings {
            dirs,
            config: OvmConfig::default(),
        };
        let names = settings.names();
        assert_eq!(names.len(), Product::ALL.len() + 1);
        assert_eq!(names.last(), Some(&SELF_ROW));
        assert_eq!(settings.policies().len(), names.len());
    }

    #[test]
    fn applying_settings_saves_only_the_rows_that_moved() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = OvmDirs::at(temp.path().join(".ovm"));
        std::fs::create_dir_all(&dirs.base).unwrap();
        let mut settings = Settings {
            dirs: OvmDirs::at(temp.path().join(".ovm")),
            config: OvmConfig::default(),
        };

        let mut chosen = settings.policies();
        let self_row = chosen.len() - 1;
        chosen[0] = AutoUpdatePolicy::Notify; // first product
        chosen[self_row] = AutoUpdatePolicy::Off; // OVM itself

        let changes = settings.apply(&chosen).unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].to, AutoUpdatePolicy::Notify);
        assert_eq!(changes[1].name, SELF_ROW);

        let reloaded = OvmConfig::load(&settings.dirs.config_file).unwrap();
        assert_eq!(
            reloaded.auto_update.policy_for(Product::ALL[0]),
            AutoUpdatePolicy::Notify
        );
        assert_eq!(reloaded.self_.auto_update, AutoUpdatePolicy::Off);
        // Untouched rows stay at the default, not pinned as overrides.
        assert_eq!(reloaded.auto_update.codex, None);
    }

    #[test]
    fn confirming_unchanged_settings_writes_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = OvmDirs::at(temp.path().join(".ovm"));
        let mut settings = Settings {
            dirs,
            config: OvmConfig::default(),
        };

        let chosen = settings.policies();
        let changes = settings.apply(&chosen).unwrap();

        assert!(changes.is_empty());
        assert!(
            !settings.dirs.config_file.exists(),
            "an untouched screen must not create or rewrite config.json"
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

    fn failure_message(results: &[(Product, Outcome)], settings_changed: bool) -> String {
        report_failures(results, settings_changed)
            .expect_err("a failed product must decide the exit code")
            .to_string()
    }

    #[test]
    fn a_clean_run_is_not_reported_as_a_failure() {
        let results = vec![(
            Product::Claude,
            Outcome::Updated {
                from: "2.1.220".into(),
                to: "2.1.224".into(),
            },
        )];

        assert!(report_failures(&results, true).is_ok());
    }

    #[test]
    fn a_partial_run_does_not_claim_nothing_changed() {
        // The exact shape of the run that exposed this: Claude installed,
        // Codex already current, Pi failed its checksum fetch. The tail must
        // not contradict the "1 updated" summary printed above it.
        let results = vec![
            (
                Product::Claude,
                Outcome::Updated {
                    from: "2.1.220".into(),
                    to: "2.1.224".into(),
                },
            ),
            (
                Product::Codex,
                Outcome::AlreadyLatest("rust-v0.146.0".into()),
            ),
            (Product::Pi, Outcome::Failed("offline".into())),
        ];

        let message = failure_message(&results, false);
        assert!(message.contains("Could not update: Pi"), "{message}");
        assert!(!message.contains("Nothing else was changed"), "{message}");
        assert!(
            message.contains("Everything else was applied as shown above"),
            "{message}"
        );
    }

    #[test]
    fn a_settings_only_change_still_counts_as_changed() {
        let results = vec![
            (
                Product::Codex,
                Outcome::AlreadyLatest("rust-v0.146.0".into()),
            ),
            (Product::Pi, Outcome::Failed("offline".into())),
        ];

        assert!(!failure_message(&results, false).contains("applied as shown above"));
        assert!(failure_message(&results, true).contains("applied as shown above"));
    }

    #[test]
    fn a_run_that_changed_nothing_says_so() {
        let results = vec![
            (
                Product::Claude,
                Outcome::Skipped {
                    detail: "not installed".into(),
                    hint: "ovm install claude latest".into(),
                },
            ),
            (Product::Pi, Outcome::Failed("offline".into())),
        ];

        let message = failure_message(&results, false);
        assert!(
            message.contains("Could not update: Pi. Nothing else was changed."),
            "{message}"
        );
    }

    #[test]
    fn policy_words_are_rejected_as_products() {
        for policy in ["on", "off", "notify"] {
            let error =
                run(Some(policy), None, None, false, false).expect_err("policy is not a product");
            assert!(
                error.to_string().contains("ovm update auto"),
                "should point at the setting form: {error}"
            );
        }
    }

    #[test]
    fn the_old_setting_shape_under_the_new_verb_is_redirected() {
        let error =
            run(Some("codex"), Some("notify"), None, false, false).expect_err("setting shape");
        assert!(
            error.to_string().contains("ovm update auto codex notify"),
            "should suggest the auto form: {error}"
        );
    }

    #[test]
    fn unknown_products_name_the_valid_subjects() {
        let error = run(Some("nope"), None, None, false, false).expect_err("unknown product");
        let text = error.to_string();
        assert!(
            text.contains("claude") && text.contains("self") && text.contains("ovm update auto")
        );
    }
}
