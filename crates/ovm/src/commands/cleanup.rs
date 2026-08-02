use crate::config::{CleanupRetention, OvmConfig, OvmDirs};
use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::version_manager::VersionManager;
use console::{style, Term};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use super::format_bytes;

/// What the launch path may do with a backlog of aged-out installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnattendedAction {
    /// Nothing is eligible.
    Skip,
    /// Too large to touch unattended: reported, left in place.
    Defer,
}

fn decide_unattended(total: usize) -> UnattendedAction {
    // A launch never removes anything, at any size. Two findings closed this
    // door for good:
    //
    // 1. Archiving was justified as "reversible", and for Codex and Pi it is
    //    not: archiving deletes the release tree while the downloaded artifact
    //    was already discarded, so only a stub remains. An offline user, or one
    //    whose upstream asset has since disappeared, cannot get it back.
    // 2. Planning and acting are separate steps, so another process running
    //    `ovm use X` between them could leave `current` pointing at a version
    //    this one just archived.
    //
    // Both vanish if the launch path is read-only. Removal happens solely in
    // `ovm cleanup now`, which revalidates the active version immediately
    // before touching each one and asks first.
    match total {
        0 => UnattendedAction::Skip,
        _ => UnattendedAction::Defer,
    }
}

/// The values of the `retention` positional that mean "do it now" rather than
/// "configure the window".
fn is_run_now_keyword(value: &str) -> bool {
    matches!(value, "now" | "run")
}

pub fn run(retention: Option<&str>) -> Result<()> {
    let dirs = OvmDirs::new()?;
    let mut config = OvmConfig::load(&dirs.config_file)?;

    match retention {
        None => {
            println!(
                "cleanup retention: {}",
                style(config.cleanup.retention.label()).green()
            );
            report_pending(&dirs, &config);
        }
        Some(value) if is_run_now_keyword(value) => {
            run_now(&dirs, &config)?;
        }
        Some(value) => {
            let retention = CleanupRetention::parse(value).ok_or_else(|| {
                OvmError::Message(
                    "Unknown cleanup retention. Use `30`, `60`, `never`, or `now` to review and \
                     remove aged-out installs."
                        .into(),
                )
            })?;
            config.set_cleanup_retention(retention);
            config.save(&dirs.config_file)?;
            println!("cleanup retention: {}", style(retention.label()).green());
        }
    }

    Ok(())
}

/// One product's pruning plan. Empty plans are never collected.
struct ProductPlan {
    vm: VersionManager,
    versions: Vec<String>,
}

/// The result of one survey pass.
struct Survey {
    /// Every non-empty product plan. Products that could not be read contribute
    /// nothing.
    plans: Vec<ProductPlan>,
    /// Whether every product was actually planned. False means at least one
    /// product's store could not be read, so the backlog reported below is a
    /// floor, not the answer.
    complete: bool,
}

/// Plan every product from one directory resolution and one config read: three
/// `VersionManager::new` calls would re-read and re-parse `config.json` three
/// times for a survey that runs on the launch path.
fn collect_plans(dirs: &OvmDirs, config: &OvmConfig, days: u64) -> Survey {
    let mut plans = Vec::new();
    let mut complete = true;

    for product in Product::ALL {
        let vm = VersionManager::with(dirs.clone(), config.clone(), product);
        match vm.plan_inactive_installs_older_than(days) {
            Ok(versions) => {
                if !versions.is_empty() {
                    plans.push(ProductPlan { vm, versions });
                }
            }
            Err(error) => {
                complete = false;
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

    Survey { plans, complete }
}

fn planned_total(plans: &[ProductPlan]) -> usize {
    plans.iter().map(|plan| plan.versions.len()).sum()
}

fn installs_label(count: usize) -> &'static str {
    if count == 1 {
        "installed version"
    } else {
        "installed versions"
    }
}

/// Where the launch-path survey records that it ran. Deliberately its own
/// stamp rather than the registry cache's freshness: that cache is written by
/// unrelated things (`ovm ls --remote`, the background refresh, the picker), so
/// hanging the survey off it would make "have we surveyed recently?" mean
/// "has anything touched the version index recently?".
fn survey_stamp_path(base: &Path) -> PathBuf {
    base.join("cleanup-checked")
}

/// Whether the launch path should survey now. Same `updateCheckInterval` the
/// version-index refresh honours, so one setting governs how often a plain
/// launch does periodic background work.
///
/// Returns the retention window to plan against, or `None` when retention is
/// off or the last survey is still fresh. Asking is deliberately free of side
/// effects: [`record_survey`] runs only once a survey has actually finished.
/// Explicit `ovm cleanup` never comes through here — a command the user typed
/// always looks.
fn due_launch_survey(base: &Path, config: &OvmConfig) -> Option<u64> {
    let days = config.cleanup.retention.days()?;
    if !survey_due(base, config.update_check_interval) {
        return None;
    }
    Some(days)
}

fn survey_due(base: &Path, interval_hours: u64) -> bool {
    if interval_hours == 0 {
        return true;
    }
    let ttl_secs = interval_hours.saturating_mul(60).saturating_mul(60);
    let stamped = std::fs::read_to_string(survey_stamp_path(base))
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok());
    // No stamp, an unreadable one, or a stamp dated after *now* all mean
    // "survey now" — the survey is read-only, so erring towards running it is
    // never destructive. That last case is a clock that moved backwards (a
    // timezone-confused restore, an NTP correction, a VM resumed from a
    // snapshot): a `now - stamped` that saturates to zero would otherwise read
    // as "surveyed a moment ago" and keep reading that way until real time
    // caught up with the future stamp plus a whole interval.
    match stamped {
        Some(stamped) => {
            let now = crate::update_cache::now_secs();
            now < stamped || now - stamped > ttl_secs
        }
        None => true,
    }
}

/// Best-effort: a stamp that cannot be written only means the next launch
/// surveys again.
///
/// Written to a unique sibling and published by `rename`, never with
/// `fs::write` at the stamp path. `fs::write` follows a symlink standing at its
/// destination, so a link planted at `~/.ovm/cleanup-checked` — pointing at any
/// file the user can write — would have an ordinary launch replace that file's
/// contents with a timestamp. `rename` unlinks or replaces whatever sits at the
/// destination rather than following it; the guarantee is pinned by
/// `version_manager::tests::a_symlink_at_the_publish_target_is_replaced_not_followed`.
fn record_survey(base: &Path) {
    let path = survey_stamp_path(base);
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(parent);

    // Unique per process *and* per call: a PID-only name collides between
    // threads of one process, and `create_new` (`O_EXCL`) then refuses the
    // second writer instead of letting two truncate one temp — or letting one
    // write through a link left at that predictable path.
    static STAMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = STAMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(".cleanup-checked-{}.{seq}.tmp", std::process::id()));

    let stamp = format!("{}\n", crate::update_cache::now_secs());
    if crate::util::write_new_file(&tmp, stamp.as_bytes()).is_err() {
        // The create may have succeeded before the write failed (a quota
        // boundary looks exactly like this); never leave scratch behind.
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if std::fs::rename(&tmp, &path).is_err() {
        // Never leave scratch behind in `~/.ovm`.
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Retention maintenance on the launch path.
///
/// A launch never removes anything: any backlog is reported and left in place
/// ([`decide_unattended`]). Planning is not free either — it stat-walks every
/// installed version of every product — so it runs at most once per
/// `updateCheckInterval`. Between surveys a plain launch does no version-store
/// walking at all, and the deferred notice repeats at that same cadence instead
/// of on every single launch.
pub(crate) fn prune_all_products(dirs: &OvmDirs, config: &OvmConfig) {
    let Some(days) = due_launch_survey(&dirs.base, config) else {
        return;
    };

    let survey = collect_plans(dirs, config, days);
    match decide_unattended(planned_total(&survey.plans)) {
        UnattendedAction::Skip => {}
        UnattendedAction::Defer => report_deferred(days, &survey.plans),
    }

    // Stamp *after* the walk, and only for a walk that saw every product.
    // Stamping up front declares a whole interval of freshness for an answer
    // that may never have been computed: a product store that could not be read,
    // or a launch killed mid-survey, would silence the backlog notice until the
    // interval expired. Stamping here means such a survey simply retries on the
    // next launch — the honest direction, since the survey is read-only and
    // repeating it costs a directory walk, never data. Retention `never` still
    // returns before any of this and leaves no stamp at all.
    if survey.complete {
        record_survey(&dirs.base);
    }
}

/// The recurring, non-destructive notice for a backlog too large to touch
/// unattended. Deliberately names them as installs, not as a cache.
fn report_deferred(days: u64, plans: &[ProductPlan]) {
    let total = planned_total(plans);
    let products = plans
        .iter()
        .map(|plan| plan.vm.product().display_name())
        .collect::<Vec<_>>()
        .join(", ");

    eprintln!(
        "  {} {} {} of {} have gone unused for {}+ days.",
        style("!").yellow().bold(),
        total,
        installs_label(total),
        products,
        days
    );
    eprintln!("    These are full product installs, not caches, so OVM will not remove this many on its own.");
    eprintln!("    Review:  {}", style("ovm list <product>").cyan());
    eprintln!("    Remove:  {}", style("ovm cleanup now").cyan());
    eprintln!("    Silence: {}", style("ovm cleanup never").cyan());
}

/// `ovm cleanup` with no argument also reports a pending backlog, so the state
/// the launch path refuses to act on is visible where retention is configured.
/// Ungated by the launch survey stamp: the user asked, so it looks.
fn report_pending(dirs: &OvmDirs, config: &OvmConfig) {
    let Some(days) = config.cleanup.retention.days() else {
        return;
    };
    let plans = collect_plans(dirs, config, days).plans;
    let total = planned_total(&plans);
    if total == 0 {
        return;
    }

    println!(
        "{} {} unused for {}+ days. Review and remove them with {}.",
        total,
        installs_label(total),
        days,
        style("ovm cleanup now").cyan()
    );
}

/// `ovm cleanup now` — the deliberate, destructive path. Lists every version
/// and its size first, then asks. Never proceeds without a terminal. Ungated by
/// the launch survey stamp for the same reason as [`report_pending`].
fn run_now(dirs: &OvmDirs, config: &OvmConfig) -> Result<()> {
    let Some(days) = config.cleanup.retention.days() else {
        return Err(OvmError::Message(
            "cleanup retention is `never`, so no install is aged out. Set a window first: ovm cleanup 30".into(),
        ));
    };

    let plans = collect_plans(dirs, config, days).plans;
    let total = planned_total(&plans);
    if total == 0 {
        println!("No installed version has been unused for {days}+ days.");
        return Ok(());
    }

    println!(
        "These {} have been unused for {days}+ days:",
        installs_label(total)
    );
    let mut total_bytes = 0u64;
    for plan in &plans {
        for candidate in plan.vm.measure_versions(&plan.versions) {
            total_bytes += candidate.bytes;
            println!(
                "  {} {} ({})",
                plan.vm.product().display_name(),
                style(&candidate.version).bold(),
                format_bytes(candidate.bytes)
            );
        }
    }
    println!(
        "Removing them deletes {} {} permanently, freeing {}.",
        total,
        installs_label(total),
        format_bytes(total_bytes)
    );

    if !confirm_removal(total)? {
        println!("Nothing removed.");
        return Ok(());
    }

    let mut total_freed = 0u64;
    let mut total_count = 0usize;
    for plan in &plans {
        let (freed, count) = plan.vm.remove_versions(&plan.versions)?;
        total_freed += freed;
        total_count += count;
    }

    println!(
        "{} Removed {} {}, freed {}",
        style("✓").green(),
        total_count,
        installs_label(total_count),
        format_bytes(total_freed)
    );
    Ok(())
}

/// Ask before deleting. A non-interactive shell (CI, a pipe, a hook) can never
/// answer, so it is told what would have happened instead of being blocked or,
/// worse, having the deletion assumed.
fn confirm_removal(total: usize) -> Result<bool> {
    if !Term::stderr().is_term() || !std::io::stdin().is_terminal() {
        return Err(OvmError::Message(format!(
            "Refusing to permanently remove {total} {} without confirmation: this shell is not interactive. \
             Re-run `ovm cleanup now` in a terminal.",
            installs_label(total)
        )));
    }

    eprint!(
        "  {} Permanently remove {} {}? {}  ",
        style("?").yellow().bold(),
        total,
        installs_label(total),
        style("[y/N]").dim()
    );
    let _ = std::io::stderr().flush();

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let reply = input.trim().to_ascii_lowercase();
    Ok(reply == "y" || reply == "yes")
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

    #[test]
    fn cleanup_parser_rejects_run_now_keywords() {
        // `now`/`run` are handled before parsing; they must never be mistaken
        // for a retention window and silently rewrite the config.
        assert_eq!(CleanupRetention::parse("now"), None);
        assert_eq!(CleanupRetention::parse("run"), None);
        assert!(is_run_now_keyword("now"));
        assert!(is_run_now_keyword("run"));
        assert!(!is_run_now_keyword("30"));
        assert!(!is_run_now_keyword("never"));
    }

    /// A config with retention on and a given check interval.
    fn config_with(retention: CleanupRetention, interval_hours: u64) -> OvmConfig {
        let mut config = OvmConfig::default();
        config.set_cleanup_retention(retention);
        config.update_check_interval = interval_hours;
        config
    }

    #[test]
    fn the_first_launch_surveys_and_the_next_one_does_not() {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path();
        let config = config_with(CleanupRetention::Days30, 24);

        // Nothing stamped yet: survey. Asking must not itself record — a survey
        // that never finishes has to be retried, not credited.
        assert_eq!(due_launch_survey(base, &config), Some(30));
        assert!(
            !survey_stamp_path(base).exists(),
            "asking whether a survey is due must not claim one ran"
        );
        assert_eq!(due_launch_survey(base, &config), Some(30));

        // Finishing one is what closes the window. The stamp is then fresh, so
        // the version store is not walked again. This is the whole point: a
        // plain launch on a 271-version store must not stat every version
        // directory to plan work it then throws away.
        record_survey(base);
        assert!(survey_stamp_path(base).exists());
        assert_eq!(due_launch_survey(base, &config), None);
    }

    #[test]
    fn a_stamp_from_the_future_surveys_instead_of_waiting_for_the_clock() {
        // A clock that moved backwards (restore, NTP correction, resumed VM)
        // leaves a stamp dated later than now. Saturating the subtraction to
        // zero would read that as "just surveyed" and keep reading it that way
        // until real time passed the future stamp plus a full interval.
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path();
        let config = config_with(CleanupRetention::Days30, 24);
        let future = crate::update_cache::now_secs().saturating_add(365 * 24 * 60 * 60);
        std::fs::write(survey_stamp_path(base), format!("{future}\n")).expect("seed stamp");

        assert_eq!(due_launch_survey(base, &config), Some(30));
    }

    /// `fs::write` at the stamp path would follow a symlink planted there and
    /// replace the contents of whatever it aims at — an unrelated user file
    /// corrupted by an ordinary launch. The stamp is published by `rename`,
    /// which replaces a link at the destination rather than following it.
    #[test]
    fn recording_a_survey_replaces_a_planted_symlink_instead_of_writing_through_it() {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path();
        let victim = base.join("precious");
        let victim_bytes = b"the file a planted link would aim at";
        std::fs::write(&victim, victim_bytes).expect("write victim");
        std::os::unix::fs::symlink(&victim, survey_stamp_path(base)).expect("plant the link");

        record_survey(base);

        assert_eq!(
            std::fs::read(&victim).expect("the victim"),
            victim_bytes,
            "the stamp must never be written through a symlink"
        );
        let stamp = survey_stamp_path(base);
        assert!(
            !stamp
                .symlink_metadata()
                .expect("stamp")
                .file_type()
                .is_symlink(),
            "the planted link must be gone, replaced by a real stamp"
        );
        assert!(std::fs::read_to_string(&stamp)
            .expect("stamp")
            .trim()
            .parse::<u64>()
            .is_ok());
        // And no scratch left behind in ~/.ovm.
        let leftovers: Vec<_> = std::fs::read_dir(base)
            .expect("read base")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn a_survey_that_could_not_read_a_product_does_not_stamp() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path().to_path_buf();
        let dirs = OvmDirs::at(base.clone());
        let config = config_with(CleanupRetention::Days30, 24);

        let versions = base.join("products/codex/versions");
        std::fs::create_dir_all(versions.join("rust-v0.1.0/release/bin")).expect("seed install");
        std::os::unix::fs::symlink(
            versions.join("rust-v0.1.0"),
            base.join("products/codex/current"),
        )
        .expect("current symlink");

        // A version directory nothing can read: planning Codex fails, so the
        // pass never saw the whole store.
        let unreadable = versions.join("rust-v0.2.0");
        std::fs::create_dir_all(&unreadable).expect("mkdir");
        let mut perms = std::fs::metadata(&unreadable)
            .expect("metadata")
            .permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&unreadable, perms).expect("chmod");
        if std::fs::read_dir(&unreadable).is_ok() {
            // Running as root, where the mode bits mean nothing.
            return;
        }

        assert!(!collect_plans(&dirs, &config, 30).complete);
        prune_all_products(&dirs, &config);
        assert!(
            !survey_stamp_path(&base).exists(),
            "an incomplete survey must not buy an interval of silence"
        );

        let mut perms = std::fs::metadata(&unreadable)
            .expect("metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&unreadable, perms).expect("restore mode");
    }

    #[test]
    fn a_stale_stamp_surveys_again() {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path();
        let config = config_with(CleanupRetention::Days60, 24);

        let aged = crate::update_cache::now_secs().saturating_sub(25 * 60 * 60);
        std::fs::write(survey_stamp_path(base), format!("{aged}\n")).expect("seed stamp");

        assert_eq!(due_launch_survey(base, &config), Some(60));
    }

    #[test]
    fn a_zero_interval_surveys_every_launch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path();
        let config = config_with(CleanupRetention::Days30, 0);

        assert_eq!(due_launch_survey(base, &config), Some(30));
        record_survey(base);
        assert_eq!(due_launch_survey(base, &config), Some(30));
    }

    #[test]
    fn retention_never_surveys_at_all_and_leaves_no_stamp() {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path();
        let config = config_with(CleanupRetention::Never, 24);

        assert_eq!(due_launch_survey(base, &config), None);
        assert!(!survey_stamp_path(base).exists());
    }

    #[test]
    fn an_unreadable_stamp_surveys_rather_than_skipping() {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path();
        let config = config_with(CleanupRetention::Days30, 24);
        std::fs::write(survey_stamp_path(base), "not-a-timestamp").expect("seed stamp");

        assert_eq!(due_launch_survey(base, &config), Some(30));
    }

    #[test]
    fn a_launch_never_removes_anything_at_any_size() {
        // Nothing eligible stays silent; ANY backlog is reported and left
        // alone. There is deliberately no size at which a launch acts: archive
        // is not reversible for every product, and planning then acting races
        // a concurrent `ovm use`.
        assert_eq!(decide_unattended(0), UnattendedAction::Skip);
        for total in [1, 2, 3, 4, 209] {
            assert_eq!(
                decide_unattended(total),
                UnattendedAction::Defer,
                "a launch must not act on {total} install(s)"
            );
        }
    }
}
