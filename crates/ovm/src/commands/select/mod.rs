mod picker;

use crate::error::{OvmError, Result};
use crate::product::Product;
#[cfg(test)]
use crate::sources::github_releases;
use crate::version_manager::{InstallRequest, VersionManager};
use console::{style, Term};
pub use picker::pick_product;
use picker::{
    interactive_select, DownloadJobs, PickerSession, ProductPick, RefreshHandle, SelectAction,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Known buddy version range (from tests/compatibility/known-features.json).
/// `/buddy` was introduced in 2.1.80 and silently removed in 2.1.97.
/// Last working version: 2.1.96.
const BUDDY_FIRST: (u64, u64, u64) = (2, 1, 80);
const BUDDY_LAST: (u64, u64, u64) = (2, 1, 96);

/// `/pet` first appears in Codex source before the first installable release.
/// The first OVM-installable release with assets is rust-v0.131.0-alpha.16.
const CODEX_PET_FIRST: &str = "0.131.0-alpha.10";

pub(super) struct VersionEntry {
    pub version: String,
    pub date: Option<String>,
    pub installed: bool,
    pub active: bool,
    pub has_companion: Option<bool>,
}

/// A row in OVM's own version picker (second level, reached from the `ovm`
/// product-picker entry). Backed by `SelfManager::list_versions`, marking the
/// active `current` and the `previous` rollback target. `dev-<hash>` snapshots
/// are flagged via [`SelfVersionRow::is_dev`].
pub(super) struct SelfVersionRow {
    pub version: String,
    pub current: bool,
    pub previous: bool,
}

impl SelfVersionRow {
    pub(super) fn is_dev(&self) -> bool {
        self.version.starts_with("dev-")
    }
}

impl VersionEntry {
    pub(super) fn is_dev_build(&self) -> bool {
        self.version.starts_with("dev:")
    }

    fn kind(&self) -> &'static str {
        if self.is_dev_build() {
            "dev"
        } else {
            "release"
        }
    }

    fn display_line(
        &self,
        version_width: usize,
        download: Option<picker::DownloadDisplay>,
    ) -> String {
        let kind_cell = format!("{:<7}", self.kind());
        let kind_str = style(kind_cell).dim().to_string();
        let version_cell = fixed_width_cell(&self.version, version_width);
        let version_str = if self.active {
            style(version_cell).green().bold().to_string()
        } else if self.installed {
            version_cell
        } else {
            style(version_cell).dim().to_string()
        };

        let date_cell = match self.date.as_deref() {
            Some(d) => format!("{d:<10}"),
            None => "—         ".to_string(),
        };
        let date_str = style(date_cell).dim().to_string();

        let companion_str = match self.has_companion {
            Some(true) => style("(^.^)").green().to_string(),
            Some(false) => style("(x.x)").dim().to_string(),
            None => String::new(),
        };

        let installed_str = match download {
            Some(picker::DownloadDisplay::Running) => format!("{}        ", style("↓").cyan()),
            Some(picker::DownloadDisplay::Failed) => format!("{}        ", style("!").yellow()),
            None if self.installed => format!("{}        ", style("✓").green()),
            None => format!("{}        ", style("—").dim()),
        };
        let active_str = if self.active {
            format!("{}     ", style("✓").green().bold())
        } else {
            format!("{}     ", style("—").dim())
        };
        format!(
            "{kind_str}  {version_str}  {date_str}  {installed_str}  {active_str}  {companion_str}"
        )
    }
}

pub(super) fn fixed_width_cell(value: &str, width: usize) -> String {
    let truncated = console::truncate_str(value, width, "…");
    console::pad_str(&truncated, width, console::Alignment::Left, None).into_owned()
}

/// Whether OVM itself should be offered in the picker, read from config.
/// Gated behind the alpha self-update channel or the `advanced.selfInPicker`
/// flag. Falls back to hidden if the config can't be resolved/loaded.
fn ovm_in_picker() -> bool {
    crate::config::OvmDirs::new()
        .ok()
        .map(|dirs| crate::config::OvmConfig::load(&dirs.config_file).unwrap_or_default())
        .map(|config| config.ovm_in_picker())
        .unwrap_or(false)
}

/// A pick's version-picker loop target: a managed product, or OVM itself.
enum Target {
    Product(Product),
    Ovm,
}

/// Entry point for `ovm select [product] [version]`.
/// If `product` is None, shows the product picker first. Esc from version picker
/// returns to product picker (so the user can switch between products). The
/// literal `ovm` product is accepted only when gated on (alpha channel or
/// `advanced.selfInPicker`); otherwise it errors with guidance.
pub fn run_top(product: Option<&str>, direct_version: Option<&str>) -> Result<()> {
    let include_ovm = ovm_in_picker();

    // Explicit `ovm select ovm [version]` — honored only when gated on, same
    // rule as the picker list. Off by default → a clear, actionable error.
    if matches!(product, Some("ovm")) {
        if !include_ovm {
            return Err(OvmError::Message(
                "OVM is not a selectable product. Enable it with `ovm self channel alpha` \
                 (or set advanced.selfInPicker=true in ~/.ovm/config.json)."
                    .into(),
            ));
        }
        if let Some(version) = direct_version {
            return select_self_direct(version);
        }
        return match run_self_version_picker(false)? {
            PickerResult::Selected | PickerResult::Back => Ok(()),
        };
    }

    let product = match product {
        Some(value) => Some(parse_product(value)?),
        None => None,
    };

    // Direct version mode: `ovm select cc 2.1.91` → switch (with install prompt if missing)
    if let (Some(p), Some(v)) = (product, direct_version) {
        let vm = VersionManager::new(p)?;
        return select_direct(&vm, v);
    }

    let from_picker = product.is_none();
    let mut target = match product {
        Some(p) => Target::Product(p),
        None => match pick_product(include_ovm)? {
            Some(ProductPick::Product(p)) => Target::Product(p),
            Some(ProductPick::Claudex) => return run_claudex_plugin(),
            Some(ProductPick::Ovm) => Target::Ovm,
            None => return Ok(()), // Quit from product picker
        },
    };

    loop {
        let result = match target {
            Target::Product(product) => {
                run_version_picker(&VersionManager::new(product)?, from_picker)?
            }
            Target::Ovm => run_self_version_picker(from_picker)?,
        };
        match result {
            PickerResult::Selected => return Ok(()),
            PickerResult::Back => {
                if from_picker {
                    // Go back to product picker
                    match pick_product(include_ovm)? {
                        Some(ProductPick::Product(p)) => target = Target::Product(p),
                        Some(ProductPick::Claudex) => return run_claudex_plugin(),
                        Some(ProductPick::Ovm) => target = Target::Ovm,
                        None => return Ok(()), // Quit
                    }
                } else {
                    // No picker to go back to — exit cleanly
                    return Ok(());
                }
            }
        }
    }
}

/// Parse a product name/alias, mirroring the CLI's `required_product` error.
fn parse_product(value: &str) -> Result<Product> {
    Product::parse(value).ok_or_else(|| {
        OvmError::Message(format!(
            "Unknown product {value}. Use one of: claude, cc, codex, cx, pi."
        ))
    })
}

enum PickerResult {
    Selected,
    Back,
}

/// Second-level picker for OVM's own versions. Lists `ovm self` versions
/// (current + previous marked, dev snapshots tagged); selecting one drives the
/// exact `ovm self use` path (atomic, next-command activation, rollback-safe).
fn run_self_version_picker(can_go_back: bool) -> Result<PickerResult> {
    let manager = crate::self_manager::SelfManager::new()?;
    let versions = manager.list_versions()?;
    if versions.is_empty() {
        return Err(OvmError::Message(
            "No self-managed OVM versions are installed.".into(),
        ));
    }
    let current = manager.current_version()?;
    let previous = manager.previous_version()?;
    let rows: Vec<SelfVersionRow> = versions
        .into_iter()
        .map(|version| SelfVersionRow {
            current: current.as_deref() == Some(version.as_str()),
            previous: previous.as_deref() == Some(version.as_str()),
            version,
        })
        .collect();

    match picker::pick_self_version(&rows, can_go_back)? {
        None => Ok(PickerResult::Back),
        Some(index) => {
            let row = &rows[index];
            if row.current {
                eprintln!(
                    "  {} Already on OVM {}",
                    style("✓").green(),
                    style(&row.version).green().bold()
                );
                return Ok(PickerResult::Selected);
            }
            // Reuse the exact CLI self-use path: operation lock, blocked
            // termination signals, atomic switch, next-command activation.
            crate::commands::self_manage::use_version(&row.version)?;
            Ok(PickerResult::Selected)
        }
    }
}

/// Direct `ovm select ovm <version>`: switch immediately if the version is
/// installed and complete, else error (no install prompt — self versions are
/// staged by `ovm self update`, not fetched on demand here).
fn select_self_direct(version: &str) -> Result<()> {
    let manager = crate::self_manager::SelfManager::new()?;
    if !manager.is_complete(version) {
        return Err(OvmError::Message(format!(
            "OVM self version {version} is not installed."
        )));
    }
    if manager.current_version()?.as_deref() == Some(version) {
        eprintln!(
            "  {} Already on OVM {}",
            style("✓").green(),
            style(version).green().bold()
        );
        return Ok(());
    }
    crate::commands::self_manage::use_version(version)
}

/// Hand off to the `ovm-claudex` plugin (first run walks through its setup;
/// afterwards it launches Claude Code against the GPT-5.6 proxy).
fn run_claudex_plugin() -> Result<()> {
    let Some(path) = crate::plugins::find_bundled("claudex") else {
        return Err(OvmError::Message(
            "claudex plugin not found — the ovm-claudex binary must be on your PATH.".into(),
        ));
    };
    let status = std::process::Command::new(path).status()?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Build the picker's master entry list from a remote/cached version list plus
/// the locally installed versions. Installed versions not present upstream are
/// appended so they never vanish. The result is fully sorted (dev builds on top,
/// newest-first) — the order the picker renders.
pub(super) fn build_entries(
    product: Product,
    installed_list: &[String],
    current: Option<&str>,
    remote: &[String],
    dates: &HashMap<String, String>,
) -> Vec<VersionEntry> {
    let installed: HashSet<&str> = installed_list.iter().map(String::as_str).collect();
    let mut entries: Vec<VersionEntry> = Vec::new();

    for version in remote.iter().rev() {
        entries.push(VersionEntry {
            installed: installed.contains(version.as_str()),
            active: current == Some(version.as_str()),
            date: dates.get(version.as_str()).cloned(),
            has_companion: check_companion_support(product, version),
            version: version.clone(),
        });
    }

    for version in installed_list.iter().rev() {
        if !remote.contains(version) {
            entries.push(VersionEntry {
                installed: true,
                active: current == Some(version.as_str()),
                date: dates.get(version.as_str()).cloned(),
                has_companion: check_companion_support(product, version),
                version: version.clone(),
            });
        }
    }

    sort_picker_entries(product, &mut entries);
    entries
}

/// Spawn a background, quiet registry refresh for `product`. The picker renders
/// what we already know instantly and folds these fresher results in when they
/// arrive. It talks only to the registry — deliberately not the chatty
/// upstream-fallback path in `list_remote_versions_with_dates`, which prints an
/// "Upstream unreachable" line — and writes the refreshed index to the cache for
/// next time. The registry client stays silent unless `OVM_VERBOSE` is set; even
/// then the picker's per-frame full-screen clear wipes any stray diagnostic line
/// on the next render, so the TUI is never left corrupted.
fn spawn_refresh(product: Product, base: PathBuf) -> RefreshHandle {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = crate::sources::registry::list_versions_from_registry(product);
        if let Some((versions, dates)) = &outcome {
            let mut sorted = versions.clone();
            product.sort_versions(&mut sorted);
            let index = crate::update_cache::VersionIndex::new(sorted, dates.clone());
            let _ = crate::update_cache::save_version_index(&base, product, &index);
        }
        let _ = tx.send(outcome);
    });
    RefreshHandle { rx }
}

fn run_version_picker(vm: &VersionManager, can_go_back: bool) -> Result<PickerResult> {
    let product = vm.product();
    let base = vm.dirs.base.clone();
    let installed_list = vm.list_installed()?;
    let current = vm.current_version()?;

    // Instant: whatever we already know from the local cache. No network here —
    // the picker paints immediately even on a wedged/absent connection.
    let cached = crate::update_cache::load_version_index(&base, product);
    let mut known: Vec<String> = cached
        .as_ref()
        .map(|i| i.versions.clone())
        .unwrap_or_default();
    let mut dates: HashMap<String, String> =
        cached.as_ref().map(|i| i.dates.clone()).unwrap_or_default();
    let mut last_checked = cached.as_ref().map(|i| i.fetched_at);
    let mut stale = cached
        .as_ref()
        .map(|i| !i.is_fresh() || (i.dates.is_empty() && !i.versions.is_empty()))
        .unwrap_or(true);

    // Only when we have literally nothing to show (brand-new install, empty
    // cache) do we fetch on the foreground — otherwise the picker would open
    // empty. This is the single blocking path, and it has nothing else to draw.
    // It runs before the alternate screen is entered, so its fallback messages
    // never land in the TUI. The fetch saves a fresh index, so no background
    // refresh is needed afterward.
    if known.is_empty() && installed_list.is_empty() {
        let (remote, remote_dates) = vm.list_remote_versions_with_dates()?;
        known = remote;
        dates = remote_dates;
        last_checked = crate::update_cache::load_version_index(&base, product)
            .map(|i| i.fetched_at)
            .or(Some(crate::update_cache::now_secs()));
        stale = false;
    }

    if known.is_empty() && installed_list.is_empty() {
        return Err(OvmError::Message(format!(
            "No versions found for {}.",
            product.display_name()
        )));
    }

    let mut entries = build_entries(product, &installed_list, current.as_deref(), &known, &dates);
    for entry in &mut entries {
        entry.installed = vm.install_is_complete(&entry.version);
        entry.active &= entry.installed;
    }

    // Stale or missing cache → kick a silent background refresh; the picker shows
    // a "checking…" hint and swaps in fresher versions live when it lands.
    let refresh = if stale {
        Some(spawn_refresh(product, base.clone()))
    } else {
        None
    };

    let mut session = PickerSession {
        product,
        installed: installed_list,
        current,
        can_go_back,
        last_checked,
        offline: false,
        refresh,
        downloads: DownloadJobs::default(),
    };

    loop {
        match interactive_select(&mut entries, &mut session)? {
            SelectAction::Cancel => return Ok(PickerResult::Back),
            SelectAction::Delete(index) => {
                let version = entries[index].version.clone();
                vm.uninstall(&version)?;
                eprintln!(
                    "  {} Removed {} {}",
                    style("✓").green(),
                    product.display_name(),
                    style(&version).bold()
                );
                mark_uninstalled(&mut entries, &version);
                session.installed.retain(|v| v != &version);
                continue;
            }
            SelectAction::Select(index) => {
                let entry = &entries[index];

                if entry.active {
                    eprintln!(
                        "  {} Already on {} {}",
                        style("✓").green(),
                        vm.product().display_name(),
                        style(&entry.version).green().bold()
                    );
                    return Ok(PickerResult::Selected);
                }

                if !entry.installed {
                    eprintln!(
                        "\n  {} Installing {} {}...",
                        style("↓").cyan(),
                        vm.product().display_name(),
                        style(&entry.version).bold()
                    );

                    let request = InstallRequest::Standard {
                        use_npm: false,
                        version: entry.version.clone(),
                    };

                    vm.install(request)?;
                }

                vm.use_version(&entry.version)?;
                super::maintain_claude_launcher(vm);
                show_happy_switch(vm.product().display_name(), &entry.version);
                super::use_version::note_pin(vm);
                let choice = prompt_launch(vm.product())?;
                launch_with_choice(vm.product(), choice)?;
                return Ok(PickerResult::Selected);
            }
        }
    }
}

/// Direct-version mode: `ovm select cc 2.1.91`.
/// If installed, switch. If not, prompt y/n to install.
fn select_direct(vm: &VersionManager, version: &str) -> Result<()> {
    let version = vm.product().normalize_version(version);

    if vm.install_is_complete(&version) {
        vm.use_version(&version)?;
        super::maintain_claude_launcher(vm);
        show_happy_switch(vm.product().display_name(), &version);
        super::use_version::note_pin(vm);
        let choice = prompt_launch(vm.product())?;
        launch_with_choice(vm.product(), choice)?;
        return Ok(());
    }

    // Not installed — prompt
    eprintln!(
        "  {} {} {} is not installed.",
        style("!").yellow(),
        vm.product().display_name(),
        style(&version).bold()
    );
    eprint!("  Install it now? [Y/n] ");
    let _ = std::io::Write::flush(&mut std::io::stderr());

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();

    if !answer.is_empty() && answer != "y" && answer != "yes" {
        eprintln!("  {} Cancelled", style("✗").dim());
        return Ok(());
    }

    eprintln!(
        "\n  {} Installing {} {}...",
        style("↓").cyan(),
        vm.product().display_name(),
        style(&version).bold()
    );

    let installed_version = vm.install(InstallRequest::Standard {
        use_npm: false,
        version: version.clone(),
    })?;
    vm.use_version(&installed_version)?;
    super::maintain_claude_launcher(vm);

    show_happy_switch(vm.product().display_name(), &installed_version);
    super::use_version::note_pin(vm);
    let choice = prompt_launch(vm.product())?;
    launch_with_choice(vm.product(), choice)?;

    Ok(())
}

/// Print Mochi the Cat with a "Now using" message on successful switch.
fn show_happy_switch(product_name: &str, version: &str) {
    let msg = format!(
        "Now using {} {}",
        product_name,
        style(version).green().bold()
    );
    eprintln!();
    for (i, line) in crate::mochi::HAPPY.lines().enumerate() {
        if i == 1 {
            eprintln!("{}  {}", style(line).green(), msg);
        } else {
            eprintln!("{}", style(line).green());
        }
    }
}

/// What the user picked at the post-switch launch prompt.
#[derive(Debug, PartialEq, Eq)]
enum LaunchChoice {
    No,
    Normal,
    Yolo,
}

/// Parse the user's reply to the launch prompt for a given product.
/// Empty or unrecognized → No. Pi has no yolo concept.
fn parse_launch_reply(product: Product, reply: &str) -> LaunchChoice {
    let trimmed = reply.trim().to_lowercase();
    if matches!(trimmed.as_str(), "y" | "yes") {
        return LaunchChoice::Normal;
    }
    let yolo_keyword = match product {
        Product::Claude => "ccy",
        Product::Codex => "cxy",
        Product::Pi => return LaunchChoice::No,
    };
    if trimmed == yolo_keyword {
        LaunchChoice::Yolo
    } else {
        LaunchChoice::No
    }
}

/// Prompt the user to launch the just-activated product. Returns the choice;
/// caller hands off to the launcher with the appropriate args. Non-interactive
/// environments (no tty) silently return No.
fn prompt_launch(product: Product) -> Result<LaunchChoice> {
    if !Term::stderr().is_term() {
        return Ok(LaunchChoice::No);
    }
    let options = match product {
        Product::Claude => "[y/n/ccy]",
        Product::Codex => "[y/n/cxy]",
        Product::Pi => "[y/n]",
    };
    eprintln!();
    eprint!(
        "  {} Launch now? {}  ",
        style("?").yellow().bold(),
        style(options).dim(),
    );
    let _ = std::io::Write::flush(&mut std::io::stderr());

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(parse_launch_reply(product, &input))
}

pub(crate) fn prompt_launch_after_switch(product: Product) -> Result<()> {
    let choice = prompt_launch(product)?;
    launch_with_choice(product, choice)
}

/// Hand off to the launcher with the choice, injecting --yolo when needed.
fn launch_with_choice(product: Product, choice: LaunchChoice) -> Result<()> {
    match choice {
        LaunchChoice::No => Ok(()),
        LaunchChoice::Normal => crate::commands::launch::run(product, &[]),
        LaunchChoice::Yolo => crate::commands::launch::run(product, &["--yolo".to_string()]),
    }
}

/// Check if a version has product-specific companion support.
fn check_companion_support(product: Product, version: &str) -> Option<bool> {
    match product {
        Product::Claude => {
            let parsed = semver::Version::parse(version).ok()?;
            let v = (parsed.major, parsed.minor, parsed.patch);
            Some(v >= BUDDY_FIRST && v <= BUDDY_LAST)
        }
        Product::Codex => {
            if version.starts_with("dev:") {
                return Some(true);
            }

            let parsed = Product::Codex.parsed_release_version(version)?;
            let first_pet = semver::Version::parse(CODEX_PET_FIRST)
                .expect("hard-coded Codex pet version should parse");
            Some(parsed >= first_pet)
        }
        Product::Pi => None,
    }
}

fn sort_picker_entries(product: Product, entries: &mut [VersionEntry]) {
    entries.sort_by(|left, right| {
        match (left.is_dev_build(), right.is_dev_build()) {
            // Dev builds always sort above releases.
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            // Codex dev builds sort ascending; everything else newest-first.
            (true, true) if matches!(product, Product::Codex) => {
                product.compare_version_strings(&left.version, &right.version)
            }
            _ => product.compare_version_strings(&right.version, &left.version),
        }
    });
}

/// Flip the `installed` flag on the matching entry. Used after the picker
/// uninstalls a version: the row should remain visible (the version is still
/// available upstream) but show as not-installed so the user can re-install.
fn mark_uninstalled(entries: &mut [VersionEntry], version: &str) {
    if let Some(entry) = entries.iter_mut().find(|e| e.version == version) {
        entry.installed = false;
        entry.active = false;
    }
}

/// Fetch release dates from GitHub. Returns version -> "YYYY-MM-DD" map.
#[cfg(test)]
fn fetch_release_dates(product: Product) -> std::collections::HashMap<String, String> {
    github_releases::get_recent_releases(product, 100)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| r.date.map(|d| (r.version, d)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{fetch_release_dates, mark_uninstalled, VersionEntry};
    use crate::product::Product;
    use mockito::Server;

    fn entry(version: &str, installed: bool, active: bool, date: Option<&str>) -> VersionEntry {
        VersionEntry {
            version: version.to_string(),
            date: date.map(String::from),
            installed,
            active,
            has_companion: None,
        }
    }

    #[test]
    fn mark_uninstalled_keeps_row_visible_with_date() {
        let mut entries = vec![
            entry("2.1.121", true, false, Some("2026-04-28")),
            entry("2.1.120", false, false, None),
            entry("2.1.119", true, true, Some("2026-04-23")),
        ];

        mark_uninstalled(&mut entries, "2.1.121");

        assert_eq!(entries.len(), 3, "row must not disappear after delete");
        let target = entries.iter().find(|e| e.version == "2.1.121").unwrap();
        assert!(!target.installed, "should be marked not-installed");
        assert!(!target.active);
        assert_eq!(
            target.date.as_deref(),
            Some("2026-04-28"),
            "date should survive uninstall"
        );
    }

    #[test]
    fn mark_uninstalled_clears_active_flag_too() {
        let mut entries = vec![entry("2.1.119", true, true, Some("2026-04-23"))];
        mark_uninstalled(&mut entries, "2.1.119");
        let e = &entries[0];
        assert!(!e.installed);
        assert!(!e.active);
    }

    #[test]
    fn mark_uninstalled_unknown_version_is_noop() {
        let mut entries = vec![entry("2.1.119", true, false, None)];
        mark_uninstalled(&mut entries, "0.0.0");
        assert!(entries[0].installed);
    }

    #[test]
    fn picker_entries_keep_codex_dev_versions_at_top() {
        let mut entries = vec![
            entry("rust-v0.130.0", false, false, Some("2026-05-16")),
            entry("rust-v0.129.0", false, false, Some("2026-05-15")),
            entry("rust-v0.135.0", true, true, None),
            entry(
                "dev:mochi-thread-unsubscribe-resume-20260513",
                true,
                false,
                None,
            ),
            entry("dev:resume-mcp-aba750e", true, true, None),
        ];

        super::sort_picker_entries(Product::Codex, &mut entries);

        let versions: Vec<&str> = entries.iter().map(|entry| entry.version.as_str()).collect();
        assert_eq!(
            versions,
            vec![
                "dev:mochi-thread-unsubscribe-resume-20260513",
                "dev:resume-mcp-aba750e",
                "rust-v0.135.0",
                "rust-v0.130.0",
                "rust-v0.129.0"
            ]
        );
    }

    #[test]
    fn codex_kind_describes_version_type_not_install_state() {
        let dev = entry("dev:resume-mcp-aba750e", true, false, None);
        let installed_without_registry_date = entry("rust-v0.135.0", true, true, None);

        assert_eq!(dev.kind(), "dev");
        assert_eq!(installed_without_registry_date.kind(), "release");
    }

    #[test]
    fn companion_support_marks_codex_pet_from_first_source_tag() {
        assert_eq!(
            super::check_companion_support(Product::Codex, "rust-v0.130.0"),
            Some(false)
        );
        assert_eq!(
            super::check_companion_support(Product::Codex, "rust-v0.131.0-alpha.9"),
            Some(false)
        );
        assert_eq!(
            super::check_companion_support(Product::Codex, "rust-v0.131.0-alpha.10"),
            Some(true)
        );
        assert_eq!(
            super::check_companion_support(Product::Codex, "rust-v0.131.0-alpha.15"),
            Some(true)
        );
        assert_eq!(
            super::check_companion_support(Product::Codex, "rust-v0.131.0-alpha.16"),
            Some(true)
        );
        assert_eq!(
            super::check_companion_support(Product::Codex, "rust-v0.131.0-alpha.21"),
            Some(true)
        );
        assert_eq!(
            super::check_companion_support(Product::Codex, "dev:resume-mcp-aba750e"),
            Some(true)
        );
        assert_eq!(super::check_companion_support(Product::Pi, "0.67.6"), None);
    }

    use super::{parse_launch_reply, LaunchChoice};

    #[test]
    fn launch_reply_y_means_normal_for_all_products() {
        for product in Product::ALL {
            assert_eq!(parse_launch_reply(product, "y"), LaunchChoice::Normal);
            assert_eq!(parse_launch_reply(product, "Y"), LaunchChoice::Normal);
            assert_eq!(parse_launch_reply(product, "yes"), LaunchChoice::Normal);
        }
    }

    #[test]
    fn launch_reply_ccy_yolo_for_claude_only() {
        assert_eq!(
            parse_launch_reply(Product::Claude, "ccy"),
            LaunchChoice::Yolo
        );
        assert_eq!(parse_launch_reply(Product::Codex, "ccy"), LaunchChoice::No);
        assert_eq!(parse_launch_reply(Product::Pi, "ccy"), LaunchChoice::No);
    }

    #[test]
    fn launch_reply_cxy_yolo_for_codex_only() {
        assert_eq!(
            parse_launch_reply(Product::Codex, "cxy"),
            LaunchChoice::Yolo
        );
        assert_eq!(parse_launch_reply(Product::Claude, "cxy"), LaunchChoice::No);
    }

    #[test]
    fn launch_reply_pi_has_no_yolo_keyword() {
        assert_eq!(parse_launch_reply(Product::Pi, "y"), LaunchChoice::Normal);
        assert_eq!(parse_launch_reply(Product::Pi, "ccy"), LaunchChoice::No);
        assert_eq!(parse_launch_reply(Product::Pi, "cxy"), LaunchChoice::No);
    }

    #[test]
    fn launch_reply_n_or_empty_means_no() {
        assert_eq!(parse_launch_reply(Product::Claude, "n"), LaunchChoice::No);
        assert_eq!(parse_launch_reply(Product::Claude, ""), LaunchChoice::No);
        assert_eq!(
            parse_launch_reply(Product::Claude, "  \n"),
            LaunchChoice::No
        );
    }

    #[test]
    fn fetch_release_dates_uses_product_specific_repo() {
        let mut server = Server::new();
        let _m = server
            .mock("GET", "/repos/openai/codex/releases?per_page=100")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[
                    {"tag_name": "rust-v0.120.0", "body": "note", "published_at": "2026-04-01T12:00:00Z"}
                ]"#,
            )
            .create();

        std::env::set_var("OVM_GITHUB_API_URL", server.url());
        let dates = fetch_release_dates(Product::Codex);
        std::env::remove_var("OVM_GITHUB_API_URL");

        assert_eq!(dates.get("rust-v0.120.0"), Some(&"2026-04-01".to_string()));
    }
}
