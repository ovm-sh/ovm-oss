use crate::config::{AutoUpdatePolicy, OvmConfig, OvmDirs};
use crate::dev_metadata::DevInstallMetadata;
use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::version_manager::{InstallRequest, VersionManager};
use std::io::Write;
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
    let product_args = apply_yolo(product, product_args)?;

    let version = match &requested_version {
        Some(version) => product.normalize_version(version),
        None => vm.current_version()?.ok_or(OvmError::NoActiveVersion)?,
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
        super::refresh_cache::spawn_all_products_if_due(&vm.dirs, &vm.config);
        super::cleanup::prune_all_products(&vm.config);
        // Under `notify` the prompt/notice replaces the generic nudge, so only
        // emit the banner for the other policies.
        if vm.config.auto_update.policy_for(product) != AutoUpdatePolicy::Notify {
            maybe_emit_update_banner(product, &version, &vm.dirs.base);
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
        // Auto-install if not present
        eprintln!(
            "  {} {} {version} not found, installing...",
            console::style("→").dim(),
            product.display_name()
        );
        let request = InstallRequest::Standard {
            use_npm: false,
            version: version.clone(),
        };
        let installed_version = vm.install(request)?;
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

fn ensure_requested_version_installed(vm: &VersionManager, version: &str) -> Result<String> {
    if vm.install_is_complete(version) {
        return Ok(version.to_string());
    }

    if version != "latest" {
        eprintln!(
            "  {} {} {version} not found, installing...",
            console::style("→").dim(),
            vm.product().display_name()
        );
    }

    let request = InstallRequest::Standard {
        use_npm: false,
        version: version.to_string(),
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

    let latest = match vm.latest_available_version() {
        Ok(latest) => latest,
        Err(error) => {
            eprintln!(
                "  {} Could not check for {} updates; launching active {} ({})",
                console::style("!").yellow(),
                vm.product().display_name(),
                active_version,
                console::style(format!("error: {error}")).dim()
            );
            return Ok(active_version.to_string());
        }
    };
    let latest = vm.product().normalize_version(&latest);
    if !vm.product().is_newer(&latest, active_version) {
        return Ok(active_version.to_string());
    }

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

    match install_and_use_latest(vm, &latest) {
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

/// Launch-time `notify` for a product: read the cached latest (no network on
/// the hot path) and, when it is newer, prompt the user (interactive) or print
/// one deduplicated notice. Install-now applies immediately before exec, exactly
/// like the `on` policy. Fail-open: any hiccup just launches the active version.
fn maybe_notify_product(vm: &VersionManager, active_version: &str) -> Result<String> {
    let product = vm.product();
    let latest = match crate::update_cache::fresh_latest(&vm.dirs.base, product) {
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

fn install_and_use_latest(vm: &VersionManager, latest: &str) -> Result<String> {
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
/// Returns the final argument list with the product's dangerous-mode flag injected
/// when yolo is active, or with `--yolo` / `--no-yolo` stripped otherwise.
fn apply_yolo(product: Product, args: Vec<&String>) -> Result<Vec<String>> {
    // Pi has no permission system — it's always unrestricted.
    // Strip --yolo/--no-yolo but don't inject any flag.
    if product == Product::Pi {
        return Ok(args
            .into_iter()
            .filter(|a| a.as_str() != "--yolo" && a.as_str() != "--no-yolo")
            .cloned()
            .collect());
    }

    let has_yolo = args.iter().any(|a| a.as_str() == "--yolo");
    let has_no_yolo = args.iter().any(|a| a.as_str() == "--no-yolo");

    let config_default = OvmDirs::new()
        .and_then(|dirs| OvmConfig::load(&dirs.config_file))
        .unwrap_or_default()
        .yolo
        .is_default(product);

    let yolo_active = (has_yolo || config_default) && !has_no_yolo;

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

    Ok(result)
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
/// `OVM_QUIET=1`, or when a dev launch is in progress.
fn maybe_emit_update_banner(product: Product, active_version: &str, base: &std::path::Path) {
    let force = std::env::var("OVM_FORCE_BANNER").is_ok_and(|v| !v.is_empty() && v != "0");
    if !force && !console::Term::stderr().is_term() {
        return;
    }
    if std::env::var("OVM_QUIET").is_ok_and(|v| !v.is_empty() && v != "0") {
        return;
    }

    let latest = crate::update_cache::fresh_latest(base, product);
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
        apply_yolo, banner_text, extract_ovm_version, format_dev_build_banner,
        is_passthrough_metadata_request, is_version_request, launch_environment,
        make_latest_default, maybe_auto_update, yolo_passthrough_flag,
    };
    use crate::config::{AutoUpdateConfig, AutoUpdatePolicy, OvmConfig, OvmDirs};
    use crate::dev_metadata::{DevInstallMetadata, DevInstallMode};
    use crate::product::Product;
    use crate::version_manager::VersionManager;
    use std::path::PathBuf;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn apply(product: Product, values: &[&str]) -> Vec<String> {
        let args = args(values);
        apply_yolo(product, args.iter().collect()).expect("apply yolo")
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
