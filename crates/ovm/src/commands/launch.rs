use crate::config::{AutoUpdatePolicy, OvmConfig};
use crate::dev_metadata::DevInstallMetadata;
use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::version_manager::{InstallRequest, VersionManager};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Launch a managed product with the active or overridden version.
pub fn run(product: Product, args: &[String]) -> Result<()> {
    let vm = VersionManager::new(product)?;
    let (requested_version, product_args) = extract_ovm_version(args)?;
    // A launch-supplied version becomes a filesystem path handed to exec; it
    // must be rejected for traversal/separators here, since the launch path
    // (unlike install/use/uninstall) otherwise reaches exec unvalidated.
    if let Some(version) = &requested_version {
        vm.reject_version_traversal(version)?;
    }
    let product_args = apply_yolo(product, product_args, &vm.config);

    let version = match &requested_version {
        Some(version) => product.normalize_version(version),
        None => match vm.current_version()? {
            Some(version) => version,
            None => match bootstrap_first_launch(&vm)? {
                Bootstrap::Managed(version) => version,
                // Nothing managed could be installed, but the machine already
                // has a working install. They asked to launch the product —
                // launch it.
                Bootstrap::Foreign(binary) => return exec_foreign(product, &binary, &product_args),
            },
        },
    };
    let version = if requested_version.is_none() {
        maybe_auto_update(&vm, &version)?
    } else {
        version
    };
    // Bare `ovm cc latest` (not `--ovm-version latest`, which stays an
    // ephemeral override) asks to move the default forward, not to pin a
    // one-off version.
    let first_arg = args.first().map(String::as_str);
    let is_bare_latest_request = first_arg == Some("latest");
    let should_prompt_after_switch = product_args.is_empty()
        && first_arg.is_some_and(|arg| arg == "latest" || looks_like_version(arg));

    if requested_version.is_none() {
        // The background refresh is spawned once per invocation from the
        // pre-dispatch hook in `main`, which covers pinned launches too.
        super::cleanup::prune_all_products(&vm.dirs, &vm.config);
        // Under `notify` the prompt/notice replaces the generic nudge, so only
        // emit the banner for the other policies.
        if vm.config.auto_update.policy_for(product) != AutoUpdatePolicy::Notify {
            maybe_emit_update_banner(product, &version, &vm.dirs.base, &vm.config);
        }
        super::self_autoupdate::maybe_notify_self_on_launch(&vm.dirs, &vm.config);
    }

    if should_prompt_after_switch {
        let version = ensure_requested_version_installed(&vm, &version)?;
        super::use_version::run(&vm, &version)?;
        // `ovm <product> latest` follows latest; a specific `ovm <product> <ver>`
        // pins it. use_version::run pinned it either way, so undo for latest.
        if is_bare_latest_request {
            vm.clear_pin();
        }
        super::use_version::note_pin(&vm);
        super::select::prompt_launch_after_switch(product)?;
        return Ok(());
    }

    let version = if requested_version.is_some() {
        let version = ensure_requested_version_installed(&vm, &version)?;
        // `ovm ccy latest` (yolo aliases inject a flag, so product_args is
        // non-empty and the prompt path above is skipped) and
        // `ovm cc latest <args>` must still make the freshly resolved version
        // the default, so plain `claude`/`codex` spawns pick it up too.
        if is_bare_latest_request {
            make_latest_default(&vm, &version)?;
        }
        version
    } else if !vm.install_is_complete(&version) {
        // Auto-install if not present. `version` is already concrete here (it
        // came from `current`, the bootstrap, or a normalized override), so the
        // helper installs without resolving anything on the network.
        let installed_version = ensure_requested_version_installed(&vm, &version)?;
        vm.use_version(&installed_version)?;
        installed_version
    } else {
        version
    };

    let binary = vm.active_binary_path(&version);

    // A newer version may have migrated the shared on-disk state DB in a way this
    // build can't read. Run optional product companions when installed (e.g.
    // Codex's `ovm-codex-skew`) before we exec it degraded — this covers every
    // spawn path (explicit `--ovm-version` pin, auto-install, auto-update, plain
    // spawn), not just `ovm use`. Skip pure metadata requests (`--version`/
    // `--help`): they print and exit without ever touching the state DB.
    // Fail-open.
    if !is_passthrough_metadata_request(&product_args) {
        // Keep ~/.local/bin/claude pointed at the managed binary (silences
        // Claude Code's startup "missing or broken" probe), and nudge if the
        // native updater is armed to reclaim control. Both Claude-only,
        // best-effort, and never block the launch.
        super::maintain_claude_launcher(&vm);
        super::nudge_if_claude_install_drift(&vm);

        crate::companions::run(
            &vm.dirs,
            product,
            crate::companions::Event::PreLaunch,
            &version,
            &binary,
        );
    }

    let dev_metadata = if version.starts_with("dev:") {
        vm.dev_install_metadata(&version)?
    } else {
        None
    };
    let version_request = is_version_request(&product_args);

    let mut command = Command::new(&binary);
    command.args(&product_args).stdin(Stdio::inherit());
    for (key, value) in launch_environment(product, &version, dev_metadata.as_ref()) {
        command.env(key, value);
    }

    if product == Product::Claude {
        command.env_remove("CLAUDECODE");
    }

    let status = if version_request && dev_metadata.is_some() {
        let output = command.output()?;
        std::io::stdout().write_all(&output.stdout)?;
        if output.status.success() {
            if let Some(metadata) = dev_metadata {
                let mut stdout = std::io::stdout();
                if !output.stdout.ends_with(b"\n") {
                    stdout.write_all(b"\n")?;
                }
                writeln!(stdout, "{}", format_dev_build_banner(&version, &metadata))?;
            }
        }
        std::io::stderr().write_all(&output.stderr)?;
        output.status
    } else {
        command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        command.status()?
    };
    std::process::exit(status.code().unwrap_or(1));
}

/// Switch the active symlinks to `version` so future plain spawns use it,
/// announcing the change unless it's already the default.
fn make_latest_default(vm: &VersionManager, version: &str) -> Result<()> {
    if vm.current_version()?.as_deref() == Some(version) {
        vm.clear_pin();
        return Ok(());
    }

    vm.use_version(version)?;
    // `ovm <product> latest` opts back into latest-tracking — clear any pin.
    vm.clear_pin();
    eprintln!(
        "  {} {} {} is now the default",
        console::style("→").dim(),
        vm.product().display_name(),
        console::style(version).green().bold()
    );
    Ok(())
}

/// What the first-launch bootstrap resolved to.
enum Bootstrap {
    /// A managed version is installed and selected — launch it as usual.
    Managed(String),
    /// Nothing could be installed (offline, upstream down, unsupported
    /// platform), but the machine has a working unmanaged install. Run that
    /// instead of failing the launch.
    Foreign(PathBuf),
}

/// Make a launch work on a machine that has never used OVM.
///
/// Asking someone to run `ovm use <product> <version>` here was a dead end:
/// nothing was installed, so that command could not succeed either, and it
/// needed a version number they had no way to know. Meanwhile a perfectly good
/// install of the product often already existed on the machine, unmanaged and
/// ignored. They asked to launch the product — so launch it.
///
/// Preference order, cheapest and least surprising first:
///   1. something already installed under OVM but not selected — just select it;
///   2. an existing unmanaged install on PATH — adopt it, so their current
///      version is preserved (a self-contained binary is imported from disk with
///      no download; a wrapper script or bundle falls back to fetching that same
///      version — see [`super::adopt::run`]);
///   3. otherwise install the latest release;
///   4. and if even that fails while an unmanaged install exists, exec that
///      binary — an offline machine with a working `claude` on PATH must not be
///      left unable to launch it.
fn bootstrap_first_launch(vm: &VersionManager) -> Result<Bootstrap> {
    let product = vm.product();

    // Installed but nothing selected: choosing for them beats an error.
    if let Some(newest) = vm.list_installed()?.into_iter().next_back() {
        eprintln!(
            "  {} No active {} version — selecting {}",
            console::style("→").dim(),
            product.display_name(),
            console::style(&newest).green().bold()
        );
        super::use_version::run(vm, &newest)?;
        vm.clear_pin();
        return Ok(Bootstrap::Managed(newest));
    }

    // Adopt what the machine already has, rather than downloading over it.
    let foreign = super::adopt::foreign_binary_on_path(&vm.dirs, product);
    if let Some(binary) = &foreign {
        eprintln!(
            "  {} First {} launch under OVM — adopting the install already on this machine",
            console::style("→").dim(),
            product.display_name()
        );
        match super::adopt::run(vm, Some(binary.clone())) {
            Ok(()) => {
                if let Some(active) = vm.current_version()? {
                    return Ok(Bootstrap::Managed(active));
                }
            }
            Err(error) => {
                // Adoption is an optimisation, not the goal. If their existing
                // binary cannot be read or parsed, fall through and install.
                eprintln!(
                    "  {} Could not adopt the existing {}: {error}",
                    console::style("!").yellow(),
                    product.display_name()
                );
            }
        }
    }

    eprintln!(
        "  {} No {} installed yet — installing the latest release",
        console::style("→").dim(),
        product.display_name()
    );
    match install_latest_and_select(vm) {
        Ok(version) => Ok(Bootstrap::Managed(version)),
        // Erroring out here used to strand a machine that had a perfectly good
        // binary on PATH: adopt could not reach the network, the latest install
        // could not either, and the launch died — with the tool they asked for
        // sitting right there. Nothing is adopted or activated, so the next
        // launch retries the managed path once the network is back.
        Err(error) => match foreign {
            Some(binary) => {
                eprintln!(
                    "  {} Could not install a managed {}: {error}",
                    console::style("!").yellow(),
                    product.display_name()
                );
                Ok(Bootstrap::Foreign(binary))
            }
            None => Err(error),
        },
    }
}

fn install_latest_and_select(vm: &VersionManager) -> Result<String> {
    let version = ensure_requested_version_installed(vm, "latest")?;
    super::use_version::run(vm, &version)?;
    // Follow latest rather than pinning: the user asked to launch, not to pin.
    vm.clear_pin();
    Ok(version)
}

/// Last-resort launch of the unmanaged binary already on PATH.
///
/// Deliberately runs it *unmanaged*: no `OVM_VERSION`/`OVM_PRODUCT` in the
/// environment and no companions, because OVM did not install this binary and
/// cannot say which version it is. The warning names the path so the launch is
/// never silently un-managed.
fn exec_foreign(product: Product, binary: &Path, args: &[String]) -> Result<()> {
    eprintln!(
        "  {} Launching the existing unmanaged {} at {}",
        console::style("!").yellow(),
        product.display_name(),
        console::style(binary.display()).dim()
    );

    let mut command = Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if product == Product::Claude {
        command.env_remove("CLAUDECODE");
    }
    let status = command.status()?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Install (if needed) a version the caller asked for by name, and return the
/// concrete version string.
///
/// `latest` is resolved here, on the network. That is the one version check a
/// launch may block on, because it is the one the user asked for by name
/// (`ovm cc latest`, or a first-launch bootstrap that has nothing to run) —
/// unlike the auto-update policy check, which reads the local cache
/// ([`cached_upgrade_target`]). Resolving before installing also means an
/// already-installed newest release costs nothing further.
fn ensure_requested_version_installed(vm: &VersionManager, version: &str) -> Result<String> {
    let version = if version == "latest" {
        eprintln!(
            "  {} Resolving latest {} version...",
            console::style("→").dim(),
            vm.product().display_name()
        );
        vm.product()
            .normalize_version(&vm.latest_available_version()?)
    } else {
        version.to_string()
    };

    if vm.install_is_complete(&version) {
        return Ok(version);
    }

    eprintln!(
        "  {} {} {version} not found, installing...",
        console::style("→").dim(),
        vm.product().display_name()
    );

    let request = InstallRequest::Standard {
        use_npm: false,
        version,
    };
    vm.install(request)
}

fn maybe_auto_update(vm: &VersionManager, active_version: &str) -> Result<String> {
    if active_version.starts_with("dev:") {
        return Ok(active_version.to_string());
    }
    match vm.config.auto_update.policy_for(vm.product()) {
        AutoUpdatePolicy::Off => return Ok(active_version.to_string()),
        AutoUpdatePolicy::Notify => return maybe_notify_product(vm, active_version),
        AutoUpdatePolicy::On => {
            // If the user deliberately pinned this exact version (`ovm switch`,
            // `ovm <product> use <version>`, or the picker), `on` must not throw
            // them to the newest release. Downgrade to notify semantics: ask on a
            // TTY (default no), or print one deduplicated notice — never a silent
            // jump. Accepting the prompt clears the pin and resumes tracking.
            if vm.read_pin().as_deref() == Some(active_version) {
                return maybe_notify_product(vm, active_version);
            }
        }
    }

    let Some(latest) = cached_upgrade_target(vm, active_version) else {
        return Ok(active_version.to_string());
    };

    crate::mochi::say(
        crate::mochi::WORKING,
        &format!(
            "Auto-updating {} {} {} {}",
            vm.product().display_name(),
            console::style(active_version).dim(),
            console::style("→").cyan(),
            console::style(&latest).green().bold(),
        ),
    );

    match install_and_use_latest_skippable(vm, &latest, active_version) {
        Ok(version) => Ok(version),
        Err(error) => {
            eprintln!(
                "  {} Auto-update to {} {} failed; launching active {} ({})",
                console::style("!").yellow(),
                vm.product().display_name(),
                console::style(&latest).bold(),
                active_version,
                console::style(format!("error: {error}")).dim()
            );
            Ok(active_version.to_string())
        }
    }
}

/// [`install_and_use_latest`] for the automatic launch path, escapable by
/// pressing Enter while the download runs.
///
/// The user never asked for this download — the `on` policy did — so it must
/// not hold the launch hostage: Enter skips it, the active version launches
/// immediately, and the update simply retries on a future launch (the cached
/// target is untouched; nothing is snoozed). Enter rather than any-key
/// because the wait polls stdin in canonical mode — deliberately so, see
/// [`crate::autoupdate::wait_child_or_skip`] for why raw mode is off the
/// table here.
///
/// The download runs as a child `ovm install` rather than in-process because a
/// skip has to actually STOP it: killing the child ends its progress output
/// before the product's TUI takes the terminal, releases the per-version
/// install lock, and leaves the incomplete install in exactly the state the
/// lock's take-over path already recovers from. The child prints the same
/// download lines this process would have, and cannot outlive this launch:
/// the guard kills and reaps it on every exit path, including unwinding.
///
/// Only the paths that cannot offer a skip fall back to the in-process
/// install: a download-free flip (nothing to wait on), a non-TTY stdin (no key
/// to press), or a failure to locate our own executable.
fn install_and_use_latest_skippable(
    vm: &VersionManager,
    latest: &str,
    active_version: &str,
) -> Result<String> {
    let needs_download = !vm.standard_install_is_complete(latest);
    let stdin_is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    let Ok(own_exe) = std::env::current_exe() else {
        return install_and_use_latest(vm, latest);
    };
    if !needs_download || !stdin_is_tty {
        return install_and_use_latest(vm, latest);
    }

    eprintln!(
        "  {} press Enter to skip and launch {} now",
        console::style("→").dim(),
        console::style(active_version).bold(),
    );
    let mut child = crate::autoupdate::KillOnDrop(
        Command::new(own_exe)
            .args(["install", vm.product().canonical_name(), latest])
            // If this launcher is killed outright (no unwind, so the guard
            // never drops), the child notices the reparenting and exits
            // rather than keep downloading into the shell's terminal.
            .env(
                crate::autoupdate::WATCH_PARENT_ENV,
                std::process::id().to_string(),
            )
            .stdin(Stdio::null())
            .spawn()?,
    );

    match crate::autoupdate::wait_child_or_skip(&mut child.0)? {
        Some(status) if status.success() => install_and_use_latest(vm, latest),
        Some(status) => Err(OvmError::Message(format!("install exited with {status}"))),
        None => {
            eprintln!(
                "  {} Update skipped — it will retry on a future launch",
                console::style("→").dim(),
            );
            Ok(active_version.to_string())
        }
    }
}

/// The version an auto-updating (`on`) launch should move to, decided from
/// local state only — the launch hot path must never fetch.
///
/// Reads the same local caches the `notify` path and the update banner read.
/// The preferred cache is a tiny aggregate-registry snapshot, validated by a
/// detached conditional ETag request that every invocation can arm *before*
/// dispatch (`main::spawn_background_refresh_if_due` → [`super::refresh_cache`]).
///
/// A cold or stale cache deliberately means "launch what is active now, upgrade
/// on the next launch" rather than "fetch now":
///   - fetching here is what made a plain `claude` block on the update service
///     on *every* launch — 15s on Claude's CDN timeout, up to ~50s on Codex's
///     npm → registry → GitHub chain — whenever the network was wedged or behind
///     a captive portal;
///   - the refresh armed by this same invocation warms the cache within seconds,
///     so `on` still converges on latest; the upgrade is deferred by one launch,
///     never skipped. This is the shape OVM's own self-update already uses:
///     stage in the background, apply at the start of the next invocation
///     ([`super::self_autoupdate`]).
///
/// Stale (older than the cache TTL) counts as cold on purpose: auto-*installing*
/// off a day-old index can resurrect a version that has since been pulled, which
/// is why `notify` reads fresh-only too.
///
/// The second candidate is a newer release **already in the version store**:
/// switching to it is a symlink flip, so it needs neither network nor download,
/// and it keeps `on` moving on a machine whose cache has gone cold.
///
/// One interaction worth knowing: `checkForUpdates: false` means no update
/// checks at all, so the cached latest is not consulted here (see
/// [`checked_latest`]) and an `on` policy will only advance as far as what is
/// already installed — a symlink flip, never a download.
fn cached_upgrade_target(vm: &VersionManager, active_version: &str) -> Option<String> {
    let product = vm.product();
    let cached = checked_latest(&vm.config, &vm.dirs.base, product)
        .map(|latest| product.normalize_version(&latest));

    [cached, newest_installed_release(vm)]
        .into_iter()
        .flatten()
        .filter(|candidate| product.is_newer(candidate, active_version))
        .max_by(|left, right| product.compare_version_strings(left, right))
}

/// The cached upstream latest, or `None` when the user turned update checks
/// off.
///
/// Every launch-time update surface — auto-update, `notify`, the banner —
/// reads the cache through here, so `checkForUpdates: false` silences all of
/// them together. Gating only the background refresh was not enough: the cache
/// is also written by explicit commands (`ovm ls --remote`, `ovm <product>
/// latest`, the picker) and survives the setting being turned off, so an `on`
/// policy could still find a fresh entry and *download* a new version on a
/// plain launch — the one thing "do not check for updates" most obviously
/// promises not to do.
///
/// This gate is about *checking*: a version the user explicitly asks for by
/// name still resolves and installs, and a newer release already in the store
/// is still selectable, because neither consults the update service.
fn checked_latest(config: &OvmConfig, base: &std::path::Path, product: Product) -> Option<String> {
    if !config.check_for_updates {
        return None;
    }
    let probed = crate::update_cache::fresh_probed_latest(base, product);
    let indexed = crate::update_cache::fresh_latest(base, product);
    match (probed, indexed) {
        (Some(probed), Some(indexed)) => {
            if product.compare_version_strings(&indexed, &probed).is_gt() {
                Some(indexed)
            } else {
                Some(probed)
            }
        }
        (Some(version), None) | (None, Some(version)) => Some(version),
        (None, None) => None,
    }
}

/// The newest complete, non-prerelease version already installed under OVM.
fn newest_installed_release(vm: &VersionManager) -> Option<String> {
    let product = vm.product();
    // `list_installed` sorts ascending, so the last match is the newest.
    vm.list_installed().ok()?.into_iter().rev().find(|version| {
        product.is_official_remote_version(version)
            && product.is_release_version(version)
            && vm.install_is_complete(version)
    })
}

/// Launch-time `notify` for a product: read the cached latest (no network on
/// the hot path) and, when it is newer, prompt the user (interactive) or print
/// one deduplicated notice. Install-now applies immediately before exec, exactly
/// like the `on` policy. Fail-open: any hiccup just launches the active version.
fn maybe_notify_product(vm: &VersionManager, active_version: &str) -> Result<String> {
    let product = vm.product();
    let latest = match checked_latest(&vm.config, &vm.dirs.base, product) {
        Some(latest) => product.normalize_version(&latest),
        None => return Ok(active_version.to_string()),
    };
    let newer = product.is_newer(&latest, active_version);
    let base = &vm.dirs.base;
    let subject = product.canonical_name();
    let is_tty = console::Term::stderr().is_term();
    let snoozed = crate::autoupdate::is_snoozed(base, subject, &latest);
    let label = format!("{} {} available", product.display_name(), latest);

    match crate::autoupdate::decide_action(AutoUpdatePolicy::Notify, newer, is_tty, snoozed) {
        crate::autoupdate::UpdateAction::Prompt => match crate::autoupdate::prompt_notify(&label) {
            crate::autoupdate::NotifyChoice::Install => {
                return install_and_use_latest(vm, &latest);
            }
            crate::autoupdate::NotifyChoice::Snooze => {
                crate::autoupdate::record_snooze(base, subject, &latest);
            }
        },
        crate::autoupdate::UpdateAction::Notice => {
            eprintln!("{label} — run `ovm {} latest`", product.shortest_alias());
            crate::autoupdate::record_snooze(base, subject, &latest);
        }
        crate::autoupdate::UpdateAction::Apply | crate::autoupdate::UpdateAction::Idle => {}
    }
    Ok(active_version.to_string())
}

/// Install `latest` if needed, activate it, and resume latest-tracking.
///
/// Shared by the launch-time `on`/`notify` paths and the imperative
/// [`super::update`] command so both converge on identical on-disk state.
pub(super) fn install_and_use_latest(vm: &VersionManager, latest: &str) -> Result<String> {
    if !vm.standard_install_is_complete(latest) {
        // The auto-update cat (printed by the caller) already announced the
        // version bump; the download then shows its own progress bar.
        vm.install(InstallRequest::Standard {
            use_npm: false,
            version: latest.to_string(),
        })?;
    }

    vm.use_version(latest)?;
    // Following latest again — drop any pin so future plain launches keep
    // auto-updating without prompting.
    vm.clear_pin();
    Ok(latest.to_string())
}

/// Resolve `--yolo` / `--no-yolo` flags and the per-product config default.
///
/// Returns the final argument list with the product's dangerous-mode flag
/// injected when yolo is active, or with `--yolo` / `--no-yolo` stripped
/// otherwise. Pi needs no special case: it has no permission system, so
/// [`yolo_passthrough_flag`] has nothing to inject and the flags are simply
/// stripped.
///
/// Takes the config the launch already loaded — re-resolving `OvmDirs` and
/// re-parsing `config.json` here made every launch read the file twice.
fn apply_yolo(product: Product, args: Vec<&String>, config: &OvmConfig) -> Vec<String> {
    let has_yolo = args.iter().any(|a| a.as_str() == "--yolo");
    let has_no_yolo = args.iter().any(|a| a.as_str() == "--no-yolo");
    let yolo_active = (has_yolo || config.yolo.is_default(product)) && !has_no_yolo;

    let mut result: Vec<String> = args
        .into_iter()
        .filter(|a| a.as_str() != "--yolo" && a.as_str() != "--no-yolo")
        .cloned()
        .collect();

    if yolo_active {
        if let Some(flag) = yolo_passthrough_flag(product) {
            result.insert(0, flag.to_string());
        }
    }

    result
}

fn yolo_passthrough_flag(product: Product) -> Option<&'static str> {
    match product {
        Product::Claude => Some("--dangerously-skip-permissions"),
        Product::Codex => Some("--dangerously-bypass-approvals-and-sandbox"),
        Product::Pi => None,
    }
}

fn extract_ovm_version(args: &[String]) -> Result<(Option<String>, Vec<&String>)> {
    let mut version = None;
    let mut remaining = Vec::new();
    let mut index = 0;

    // Detect bare "latest" or version string as first arg (e.g. `ovm cc latest`, `ovm cc 2.1.91`)
    if let Some(first) = args.first() {
        if first == "latest" || looks_like_version(first) {
            return Ok((Some(first.clone()), args[1..].iter().collect()));
        }
    }

    while let Some(arg) = args.get(index) {
        if let Some(value) = arg.strip_prefix("--ovm-version=") {
            version = Some(value.to_string());
            index += 1;
        } else if arg == "--ovm-version" {
            // The value is a separate token. Reject a missing or option-like
            // one (e.g. `--ovm-version --model x`) instead of consuming the
            // following application option as the version — no version string
            // begins with `-`.
            let value = match args.get(index + 1) {
                Some(value) if !value.starts_with('-') => value,
                _ => {
                    return Err(OvmError::Message(
                        "--ovm-version requires a version.".into(),
                    ))
                }
            };
            version = Some(value.clone());
            index += 2;
        } else {
            remaining.push(arg);
            index += 1;
        }
    }

    Ok((version, remaining))
}

/// Check if a string looks like a version (e.g. "2.1.91", "v0.120.0", "rust-v0.120.0", "dev:foo")
fn looks_like_version(s: &str) -> bool {
    s.starts_with("dev:")
        || s.starts_with("rust-v")
        || s.starts_with('v')
            && s.len() > 1
            && s.as_bytes().get(1).is_some_and(|b| b.is_ascii_digit())
        || s.chars().next().is_some_and(|c| c.is_ascii_digit()) && s.contains('.')
}

fn is_version_request(args: &[impl AsRef<str>]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_ref(), "--version" | "-V"))
}

fn is_passthrough_metadata_request(args: &[impl AsRef<str>]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_ref(), "--version" | "-V" | "--help" | "-h"))
}

fn launch_environment(
    product: Product,
    version: &str,
    dev_metadata: Option<&DevInstallMetadata>,
) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("OVM_PRODUCT", product.canonical_name().to_string()),
        ("OVM_VERSION", version.to_string()),
    ];

    let is_dev_build =
        product == Product::Codex && (version.starts_with("dev:") || dev_metadata.is_some());
    if is_dev_build {
        env.push(("OVM_DEV_BUILD", "1".to_string()));
    }

    env
}

/// If a fresh cache entry says a newer upstream version exists than what's active,
/// print a one-line nudge to stderr. Suppressed when stderr isn't a tty, when
/// `OVM_QUIET=1`, when a dev launch is in progress, or when the user turned
/// update checks off.
fn maybe_emit_update_banner(
    product: Product,
    active_version: &str,
    base: &std::path::Path,
    config: &OvmConfig,
) {
    let force = std::env::var("OVM_FORCE_BANNER").is_ok_and(|v| !v.is_empty() && v != "0");
    if !force && !console::Term::stderr().is_term() {
        return;
    }
    if std::env::var("OVM_QUIET").is_ok_and(|v| !v.is_empty() && v != "0") {
        return;
    }

    let latest = checked_latest(config, base, product);
    if let Some(text) = banner_text(product, active_version, latest.as_deref()) {
        eprintln!("{text}");
    }
}

/// Pure function: decide whether to emit a banner and what text to use.
fn banner_text(product: Product, active_version: &str, latest: Option<&str>) -> Option<String> {
    if active_version.starts_with("dev:") {
        return None;
    }
    let latest = latest?;
    let normalized = product.normalize_version(latest);
    if normalized == active_version {
        return None;
    }
    if !product.is_newer(&normalized, active_version) {
        return None;
    }

    Some(format!(
        "{} {} {} available. Run: {}",
        console::style("(≈^.^≈)").dim(),
        product.display_name(),
        console::style(&normalized).bold(),
        console::style(format!("ovm {} latest", product.shortest_alias())).cyan(),
    ))
}

fn format_dev_build_banner(version: &str, metadata: &DevInstallMetadata) -> String {
    let mut details = vec![metadata.mode.label().to_string()];
    if let Some(branch) = metadata.git_branch.as_deref() {
        details.push(format!("branch={branch}"));
    }
    if let Some(commit) = metadata.git_commit.as_deref() {
        details.push(format!("commit={commit}"));
    }

    format!("ovm dev build: {version} ({})", details.join(", "))
}

#[cfg(test)]
mod tests {
    use super::{
        apply_yolo, banner_text, cached_upgrade_target, checked_latest, extract_ovm_version,
        format_dev_build_banner, is_passthrough_metadata_request, is_version_request,
        launch_environment, make_latest_default, maybe_auto_update, maybe_notify_product,
        yolo_passthrough_flag,
    };
    use crate::config::{AutoUpdateConfig, AutoUpdatePolicy, OvmConfig, OvmDirs};
    use crate::dev_metadata::{DevInstallMetadata, DevInstallMode};
    use crate::product::Product;
    use crate::update_cache::{save_version_index, VersionIndex};
    use crate::version_manager::VersionManager;
    use std::path::PathBuf;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn apply(product: Product, values: &[&str]) -> Vec<String> {
        apply_with(product, values, &OvmConfig::default())
    }

    fn apply_with(product: Product, values: &[&str], config: &OvmConfig) -> Vec<String> {
        let args = args(values);
        apply_yolo(product, args.iter().collect(), config)
    }

    #[test]
    fn the_config_default_injects_yolo_without_a_flag() {
        // The default now comes from the config the launch already loaded, so
        // it is finally testable without touching the real `~/.ovm`.
        let mut config = OvmConfig::default();
        config.yolo.claude = true;

        assert_eq!(
            apply_with(Product::Claude, &["hello"], &config),
            vec![
                "--dangerously-skip-permissions".to_string(),
                "hello".to_string(),
            ]
        );
        // `--no-yolo` still beats the config default.
        assert_eq!(
            apply_with(Product::Claude, &["--no-yolo", "hello"], &config),
            vec!["hello".to_string()]
        );
    }

    #[test]
    fn codex_yolo_uses_current_bypass_flag() {
        assert_eq!(
            apply(Product::Codex, &["--yolo", "hello"]),
            vec![
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
                "hello".to_string(),
            ]
        );
    }

    #[test]
    fn claude_yolo_keeps_skip_permissions_flag() {
        assert_eq!(
            apply(Product::Claude, &["--yolo", "hello"]),
            vec![
                "--dangerously-skip-permissions".to_string(),
                "hello".to_string(),
            ]
        );
    }

    #[test]
    fn pi_strips_yolo_without_injecting_a_flag() {
        assert_eq!(apply(Product::Pi, &["--yolo", "hello"]), vec!["hello"]);
    }

    #[test]
    fn no_yolo_strips_yolo_and_prevents_injection() {
        assert_eq!(
            apply(Product::Codex, &["--yolo", "--no-yolo", "hello"]),
            vec!["hello"]
        );
    }

    #[test]
    fn dangerous_mode_flags_are_product_specific() {
        assert_eq!(
            yolo_passthrough_flag(Product::Codex),
            Some("--dangerously-bypass-approvals-and-sandbox")
        );
        assert_eq!(
            yolo_passthrough_flag(Product::Claude),
            Some("--dangerously-skip-permissions")
        );
        assert_eq!(yolo_passthrough_flag(Product::Pi), None);
    }

    #[test]
    fn version_request_detects_common_flags() {
        let version_args = ["--version".to_string()];
        let help_args = ["--help".to_string()];
        let short_args = ["-V".to_string()];

        assert!(is_version_request(&version_args.iter().collect::<Vec<_>>()));
        assert!(is_version_request(&short_args.iter().collect::<Vec<_>>()));
        assert!(!is_version_request(&help_args.iter().collect::<Vec<_>>()));
    }

    #[test]
    fn passthrough_metadata_request_detects_help_and_version_flags() {
        assert!(is_passthrough_metadata_request(&["--help".to_string()]));
        assert!(is_passthrough_metadata_request(&["-h".to_string()]));
        assert!(is_passthrough_metadata_request(&["--version".to_string()]));
        assert!(!is_passthrough_metadata_request(&["exec".to_string()]));
    }

    #[test]
    fn dev_build_banner_includes_branch_and_commit_when_present() {
        let metadata = DevInstallMetadata {
            kind: "dev".to_string(),
            mode: DevInstallMode::Link,
            source: PathBuf::from("/tmp/codex"),
            git_repo_root: Some(PathBuf::from("/tmp/repo")),
            git_branch: Some("feature/resume-fix".to_string()),
            git_commit: Some("abc123def456".to_string()),
        };

        assert_eq!(
            format_dev_build_banner("dev:resume-fix", &metadata),
            "ovm dev build: dev:resume-fix (link, branch=feature/resume-fix, commit=abc123def456)"
        );
    }

    #[test]
    fn extract_ovm_version_supports_equals_syntax() {
        let args = vec![
            "--ovm-version=rust-v0.118.0".to_string(),
            "--version".to_string(),
        ];

        let (version, remaining) = extract_ovm_version(&args).expect("extract version");

        assert_eq!(version.as_deref(), Some("rust-v0.118.0"));
        assert_eq!(remaining, vec![&args[1]]);
    }

    #[test]
    fn extract_ovm_version_supports_separate_value_syntax() {
        let args = vec![
            "--ovm-version".to_string(),
            "rust-v0.118.0".to_string(),
            "exec".to_string(),
        ];

        let (version, remaining) = extract_ovm_version(&args).expect("extract version");

        assert_eq!(version.as_deref(), Some("rust-v0.118.0"));
        assert_eq!(remaining, vec![&args[2]]);
    }

    #[test]
    fn extract_ovm_version_requires_value() {
        let args = vec!["--ovm-version".to_string()];

        let error = extract_ovm_version(&args).expect_err("missing value");

        assert_eq!(error.to_string(), "--ovm-version requires a version.");
    }

    #[test]
    fn extract_ovm_version_rejects_option_like_value() {
        // `cc --ovm-version --model x` must not swallow `--model` as the
        // version; it errors instead of silently selecting an invalid one.
        for next in ["--model", "-m", "--"] {
            let args = vec![
                "--ovm-version".to_string(),
                next.to_string(),
                "sonnet".to_string(),
            ];
            let error = extract_ovm_version(&args).expect_err("option-like value");
            assert_eq!(error.to_string(), "--ovm-version requires a version.");
        }
    }

    #[test]
    fn launch_environment_marks_codex_dev_builds() {
        let env = launch_environment(Product::Codex, "dev:resume-fix", None);

        assert!(env.contains(&("OVM_PRODUCT", "codex".to_string())));
        assert!(env.contains(&("OVM_VERSION", "dev:resume-fix".to_string())));
        assert!(env.contains(&("OVM_DEV_BUILD", "1".to_string())));
    }

    #[test]
    fn launch_environment_marks_release_builds_without_dev_flag() {
        let env = launch_environment(Product::Codex, "rust-v0.120.0", None);

        assert!(env.contains(&("OVM_PRODUCT", "codex".to_string())));
        assert!(env.contains(&("OVM_VERSION", "rust-v0.120.0".to_string())));
        assert!(!env.iter().any(|(key, _)| *key == "OVM_DEV_BUILD"));
    }

    #[test]
    fn launch_environment_marks_other_products_without_dev_flag() {
        let env = launch_environment(Product::Claude, "2.1.91", None);

        assert!(env.contains(&("OVM_PRODUCT", "claude".to_string())));
        assert!(env.contains(&("OVM_VERSION", "2.1.91".to_string())));
        assert!(!env.iter().any(|(key, _)| *key == "OVM_DEV_BUILD"));
    }

    #[test]
    fn banner_emitted_when_latest_is_newer() {
        let text = banner_text(Product::Claude, "2.1.85", Some("2.1.91"))
            .expect("banner should be emitted");
        let plain = console::strip_ansi_codes(&text).to_string();
        assert!(plain.contains("(≈^.^≈)"));
        assert!(plain.contains("Claude Code"));
        assert!(plain.contains("2.1.91"));
        assert!(plain.contains("Run: ovm cc latest"));
    }

    #[test]
    fn banner_suppressed_when_active_matches_latest() {
        assert!(banner_text(Product::Claude, "2.1.91", Some("2.1.91")).is_none());
    }

    #[test]
    fn banner_suppressed_when_active_is_newer() {
        assert!(banner_text(Product::Claude, "2.2.0", Some("2.1.91")).is_none());
    }

    #[test]
    fn banner_suppressed_when_no_cache_entry() {
        assert!(banner_text(Product::Claude, "2.1.85", None).is_none());
    }

    #[test]
    fn banner_suppressed_for_dev_versions() {
        assert!(banner_text(Product::Codex, "dev:resume-fix", Some("rust-v0.120.0")).is_none());
    }

    #[test]
    fn banner_uses_cx_alias_for_codex() {
        let text =
            banner_text(Product::Codex, "rust-v0.118.0", Some("rust-v0.120.0")).expect("banner");
        let plain = console::strip_ansi_codes(&text).to_string();
        assert!(plain.contains("Run: ovm cx latest"));
    }

    /// Build a Claude VersionManager rooted at `base` with fake installed versions.
    fn seeded_claude_vm(base: &std::path::Path, versions: &[&str]) -> VersionManager {
        let dirs = OvmDirs::at(base.to_path_buf());
        let vm = VersionManager {
            product_dirs: dirs.product_dirs(Product::Claude),
            dirs,
            config: OvmConfig::default(),
        };
        for version in versions {
            let bin = vm.product_dirs.native_bin(version);
            std::fs::create_dir_all(bin.parent().expect("bin parent")).expect("mkdir");
            std::fs::write(&bin, "#!/bin/sh\n").expect("write fake binary");
            std::fs::write(bin.parent().expect("native root").join(".complete"), "")
                .expect("write completion marker");
        }
        vm
    }

    #[test]
    fn make_latest_default_switches_current_symlink() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vm = seeded_claude_vm(temp.path(), &["2.1.159", "2.1.170"]);
        vm.use_version("2.1.159").expect("seed current");

        make_latest_default(&vm, "2.1.170").expect("make default");

        assert_eq!(
            vm.current_version().expect("current"),
            Some("2.1.170".into())
        );
    }

    #[test]
    fn launch_rejects_versions_that_escape_the_store() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vm = seeded_claude_vm(temp.path(), &["2.1.170"]);
        for evil in [
            "/tmp/evil",
            "../../../../tmp/evil",
            "2.1.170/../../../tmp/evil",
            "foo/bar",
        ] {
            assert!(
                vm.reject_version_traversal(evil).is_err(),
                "must reject `{evil}` before it reaches exec"
            );
        }
        // Legitimate installed/dev/pinned versions still pass.
        for ok in ["2.1.170", "dev:resume", "rust-v0.44.0"] {
            assert!(
                vm.reject_version_traversal(ok).is_ok(),
                "`{ok}` should pass"
            );
        }
    }

    #[test]
    fn make_latest_default_is_noop_when_already_default() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vm = seeded_claude_vm(temp.path(), &["2.1.170"]);
        vm.use_version("2.1.170").expect("seed current");

        make_latest_default(&vm, "2.1.170").expect("make default");

        assert_eq!(
            vm.current_version().expect("current"),
            Some("2.1.170".into())
        );
    }

    /// Write a version index for `product` whose entries are `versions`, aged
    /// `age_secs` seconds. Age is what separates a usable cache from a stale one.
    fn seed_version_index(base: &std::path::Path, product: Product, versions: &[&str], age: u64) {
        let index = VersionIndex {
            versions: versions.iter().map(|value| value.to_string()).collect(),
            dates: std::collections::HashMap::new(),
            fetched_at: crate::update_cache::now_secs().saturating_sub(age),
        };
        save_version_index(base, product, &index).expect("seed version index");
    }

    #[test]
    fn upgrade_target_comes_from_the_fresh_cache() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vm = seeded_claude_vm(temp.path(), &["2.1.159"]);
        seed_version_index(
            temp.path(),
            Product::Claude,
            &["2.1.159", "2.1.170"],
            60 * 60,
        );

        assert_eq!(
            cached_upgrade_target(&vm, "2.1.159").as_deref(),
            Some("2.1.170")
        );
    }

    #[test]
    fn upgrade_target_prefers_the_lightweight_aggregate_probe() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vm = seeded_claude_vm(temp.path(), &["2.1.159"]);
        seed_version_index(temp.path(), Product::Claude, &["2.1.159"], 60 * 60);
        let summary = crate::update_cache::RegistryProductSummary {
            latest: "2.1.170".into(),
            version_count: 170,
            retired_count: 0,
            updated_at: "2026-08-19T01:22:51Z".into(),
        };
        crate::update_cache::record_latest_probe_modified(
            temp.path(),
            Some("\"registry-v2\"".into()),
            std::collections::HashMap::from([(Product::Claude, summary)]),
            crate::update_cache::now_secs(),
        )
        .expect("seed aggregate probe");

        assert_eq!(
            cached_upgrade_target(&vm, "2.1.159").as_deref(),
            Some("2.1.170")
        );
    }

    #[test]
    fn upgrade_target_uses_a_newer_full_index_over_an_older_probe() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vm = seeded_claude_vm(temp.path(), &["2.1.159"]);
        seed_version_index(temp.path(), Product::Claude, &["2.1.171"], 60 * 60);
        let summary = crate::update_cache::RegistryProductSummary {
            latest: "2.1.170".into(),
            version_count: 170,
            retired_count: 0,
            updated_at: "2026-08-19T01:22:51Z".into(),
        };
        crate::update_cache::record_latest_probe_modified(
            temp.path(),
            Some("\"registry-v2\"".into()),
            std::collections::HashMap::from([(Product::Claude, summary)]),
            crate::update_cache::now_secs(),
        )
        .expect("seed aggregate probe");
        crate::update_cache::record_index_refresh_success(temp.path(), Product::Claude)
            .expect("mark full index current");

        assert_eq!(
            cached_upgrade_target(&vm, "2.1.159").as_deref(),
            Some("2.1.171")
        );
    }

    #[test]
    fn upgrade_target_is_none_when_the_cache_is_cold() {
        // A machine that has never refreshed, with nothing newer on disk. The
        // launch must NOT fetch to fill the gap: the detached refresh armed by
        // this same invocation warms the cache, and the upgrade lands on the
        // next launch.
        let temp = tempfile::tempdir().expect("tempdir");
        let vm = seeded_claude_vm(temp.path(), &["2.1.159"]);

        assert_eq!(cached_upgrade_target(&vm, "2.1.159"), None);
    }

    #[test]
    fn upgrade_target_ignores_a_stale_cache() {
        // Older than the index TTL. Auto-*installing* off a day-old index can
        // resurrect a pulled release, so stale counts as cold — same rule the
        // notify path follows.
        let temp = tempfile::tempdir().expect("tempdir");
        let vm = seeded_claude_vm(temp.path(), &["2.1.159"]);
        seed_version_index(
            temp.path(),
            Product::Claude,
            &["2.1.159", "2.1.170"],
            48 * 60 * 60,
        );

        assert_eq!(cached_upgrade_target(&vm, "2.1.159"), None);
    }

    #[test]
    fn upgrade_target_uses_a_newer_release_that_is_already_installed() {
        // No cache at all, but the store already holds a newer release: that
        // upgrade is a symlink flip, so `on` takes it without any network.
        let temp = tempfile::tempdir().expect("tempdir");
        let vm = seeded_claude_vm(temp.path(), &["2.1.159", "2.1.170"]);

        assert_eq!(
            cached_upgrade_target(&vm, "2.1.159").as_deref(),
            Some("2.1.170")
        );
    }

    #[test]
    fn upgrade_target_is_none_when_active_is_already_newest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vm = seeded_claude_vm(temp.path(), &["2.1.159"]);
        seed_version_index(temp.path(), Product::Claude, &["2.1.159"], 60 * 60);

        assert_eq!(cached_upgrade_target(&vm, "2.1.159"), None);
        assert_eq!(cached_upgrade_target(&vm, "2.1.170"), None);
    }

    /// `checkForUpdates: false` must mean no update checks *anywhere*, not
    /// just no background refresh. A cache warmed before the setting was
    /// turned off (or by an explicit `ovm ls --remote`) previously let a plain
    /// launch under `on` download a version the user never asked for.
    #[test]
    fn update_checks_off_hides_the_cached_latest_from_every_surface() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut vm = seeded_claude_vm(temp.path(), &["2.1.159"]);
        seed_version_index(
            temp.path(),
            Product::Claude,
            &["2.1.159", "2.1.170"],
            60 * 60,
        );

        // With checks on, the fresh cache is visible — the control.
        assert_eq!(
            checked_latest(&vm.config, &vm.dirs.base, Product::Claude).as_deref(),
            Some("2.1.170")
        );

        vm.config.check_for_updates = false;
        assert_eq!(
            checked_latest(&vm.config, &vm.dirs.base, Product::Claude),
            None
        );
        assert_eq!(
            cached_upgrade_target(&vm, "2.1.159"),
            None,
            "an `on` launch must not download off a cache the user asked us not to fill"
        );
        assert_eq!(
            maybe_notify_product(&vm, "2.1.159").expect("notify"),
            "2.1.159",
            "`notify` must not advertise an update either"
        );
    }

    /// The gate is about *checking*, not about pinning the machine: a newer
    /// release already in the store is a symlink flip with no network, so `on`
    /// still takes it with update checks off.
    #[test]
    fn update_checks_off_still_selects_a_newer_installed_release() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut vm = seeded_claude_vm(temp.path(), &["2.1.159", "2.1.170"]);
        vm.config.check_for_updates = false;

        assert_eq!(
            cached_upgrade_target(&vm, "2.1.159").as_deref(),
            Some("2.1.170")
        );
    }

    #[test]
    fn auto_update_on_with_a_cold_cache_launches_the_active_version() {
        // The default policy (`on`) against a machine with no cached index and
        // nothing newer installed: the active version is returned untouched,
        // without a network round trip.
        let temp = tempfile::tempdir().expect("tempdir");
        let vm = seeded_claude_vm(temp.path(), &["2.1.159"]);
        vm.use_version("2.1.159").expect("seed current");

        assert_eq!(
            maybe_auto_update(&vm, "2.1.159").expect("auto update"),
            "2.1.159"
        );
    }

    #[test]
    fn auto_update_skips_dev_versions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = OvmDirs::at(temp.path().to_path_buf());
        let vm = VersionManager {
            product_dirs: dirs.product_dirs(Product::Codex),
            dirs,
            config: OvmConfig {
                auto_update: AutoUpdateConfig {
                    default: AutoUpdatePolicy::On,
                    ..AutoUpdateConfig::default()
                },
                ..OvmConfig::default()
            },
        };

        let version = maybe_auto_update(&vm, "dev:resume").expect("auto update");
        assert_eq!(version, "dev:resume");
    }
}
