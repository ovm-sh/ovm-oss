mod autoupdate;
mod bundle_manifest;
mod claude_install;
mod cli;
mod commands;
mod companions;
mod config;
mod dev_metadata;
mod error;
mod hooks;
mod mochi;
mod node;
mod plugins;
mod product;
mod release_metadata;
mod self_manager;
mod sources;
mod symlink;
mod update_cache;
mod util;
mod version_manager;

use clap::Parser;
use cli::{Cli, Commands, SelfCommands};
use error::{OvmError, Result};
use product::Product;
use std::path::Path;
use std::path::PathBuf;
use version_manager::{DevInstallSource, InstallRequest, VersionManager};

fn main() {
    if let Err(error) = run() {
        abort(error);
    }
}

fn run() -> Result<()> {
    if self_manager::run_lock_helper_if_requested()? {
        return Ok(());
    }
    let args: Vec<String> = std::env::args().collect();
    // Activate a self-update staged by an earlier launch (auto-update policy
    // `on`) before dispatching: the atomic swap happens here, at the start of
    // this next invocation, so the exec below picks up the new version. Cheap
    // and fail-open — a missing pending marker is a single stat, and nothing it
    // does can break a launch.
    commands::self_autoupdate::activate_pending_on_startup(&args);
    // A direct install keeps a stable control-plane binary at ~/.ovm/bin/ovm.
    // It handles self-management locally and execs every other invocation into
    // the atomically selected immutable OVM version. Proxy before claiming the
    // claudex lease so the selected child remains the process that owns it.
    self_manager::proxy_if_needed(&args)?;
    claim_claudex_session_lease()?;

    // The hidden background-refresh command is dispatched by its sentinel
    // argument *before* any name-based routing. When ovm is installed as the
    // `pi` owned launcher (argv[0] == "pi"), the launcher interception below
    // would otherwise route `__refresh-cache` into a real Pi launch — which
    // re-arms the refresh spawner without ever clearing the "due" flag,
    // producing a self-perpetuating fork storm.
    if args.get(1).map(String::as_str) == Some("__refresh-cache") {
        commands::refresh_cache::run_hidden()?;
        return Ok(());
    }

    if let Some(product) = invoked_as_product_launcher(args.first()) {
        match product {
            Product::Pi => commands::pi::run(&args[1..])?,
            Product::Codex => commands::codex::run(&args[1..])?,
            Product::Claude => commands::claude::run(&args[1..])?,
        }
        return Ok(());
    }

    // Bare `ovm` with no args → short help (hint at `ovm help` for more).
    if args.len() == 1 {
        return commands::help::run_short();
    }

    if let Some(command) = args.get(1).map(String::as_str) {
        match command {
            "help" if args.len() == 2 => {
                commands::help::run()?;
                return Ok(());
            }
            "claude" | "cc" => {
                commands::claude::run(&args[2..])?;
                return Ok(());
            }
            "ccy" => {
                run_yolo_launch(Product::Claude, &args[2..])?;
                return Ok(());
            }
            "codex" | "cx" => {
                commands::codex::run(&args[2..])?;
                return Ok(());
            }
            "cxy" => {
                run_yolo_launch(Product::Codex, &args[2..])?;
                return Ok(());
            }
            // Codex fast mode: inject the priority service tier via `-c`.
            // `cxf` = fast, `cxyf` = fast + yolo. "priority" is the wire
            // value ("fast" is the ChatGPT-account alias for it, but Azure
            // rejects that spelling — "priority" is accepted everywhere).
            "cxf" => {
                run_fast_codex(&args[2..], false)?;
                return Ok(());
            }
            "cxyf" => {
                run_fast_codex(&args[2..], true)?;
                return Ok(());
            }
            // claudex: Claude Code UI on GPT-5.6 via the ovm-claudex plugin.
            // ccx follows the cc/cx naming; suffix `y` = yolo, `f` = fast
            // (OpenAI priority service tier for main + subagents). Both
            // suffixes stack: ccxyf = claudex, yolo, fast.
            "ccx" => {
                run_claudex(&args[2..], false, false)?;
                return Ok(());
            }
            "ccxy" => {
                run_claudex(&args[2..], true, false)?;
                return Ok(());
            }
            "ccxf" => {
                run_claudex(&args[2..], false, true)?;
                return Ok(());
            }
            "ccxyf" => {
                run_claudex(&args[2..], true, true)?;
                return Ok(());
            }
            "pi" => {
                commands::pi::run(&args[2..])?;
                return Ok(());
            }
            _ => {
                // Try plugin dispatch: `ovm <name>` → exec `ovm-<name>` if it exists on PATH.
                // Only attempt for names that don't match built-in clap subcommands
                // to avoid accidentally shadowing them.
                if !is_builtin_command(command) {
                    if let Some(plugin_path) = plugins::find_for_dispatch(command) {
                        let status = plugins::dispatch(&plugin_path, &args[2..])?;
                        std::process::exit(status.code().unwrap_or(1));
                    }
                }
            }
        }
    }

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => return Err(format_clap_error(err)),
    };
    let result = match cli.command {
        Commands::List {
            product,
            remote,
            all,
        } => {
            let vm = VersionManager::new(required_product(&product)?)?;
            let scope = if all {
                commands::list::Scope::All
            } else if remote {
                commands::list::Scope::Remote
            } else {
                commands::list::Scope::Installed
            };
            commands::list::run(&vm, scope)
        }
        Commands::Current { product } => match product {
            Some(p) => commands::current::run(&VersionManager::new(required_product(&p)?)?),
            None => commands::current::run_all(),
        },
        Commands::Which { product } => match product {
            Some(p) => commands::which::run(&VersionManager::new(required_product(&p)?)?),
            None => commands::which::run_all(),
        },
        Commands::Use { product, version } => {
            let vm = VersionManager::new(required_product(&product)?)?;
            commands::use_version::run(&vm, &version)?;
            commands::use_version::note_pin(&vm);
            Ok(())
        }
        Commands::Adopt { product, path } => {
            commands::adopt::run(&VersionManager::new(required_product(&product)?)?, path)
        }
        Commands::Uninstall { product, version } => {
            commands::uninstall::run(&VersionManager::new(required_product(&product)?)?, &version)
        }
        Commands::Clean {
            product,
            version,
            all,
        } => {
            let product = required_product(&product)?;
            commands::clean::run(&VersionManager::new(product)?, version.as_deref(), all)
        }
        Commands::Cleanup { retention } => commands::cleanup::run(retention.as_deref()),
        Commands::Archive {
            product,
            version,
            below,
        } => {
            let product = required_product(&product)?;
            commands::archive::run(
                &VersionManager::new(product)?,
                version.as_deref(),
                below.as_deref(),
            )
        }
        Commands::Install {
            product,
            version,
            npm,
            dev,
            binary,
            bundle,
            link,
        } => {
            let (product, request) =
                resolve_install_request(&product, &version, npm, dev, binary, bundle, link)?;
            commands::install::run(&VersionManager::new(product)?, request)
        }
        Commands::Stats => commands::stats::run(),
        Commands::Select { product, version } => {
            commands::select::run_top(product.as_deref(), version.as_deref())
        }
        Commands::Info { product, version } => {
            let vm = VersionManager::new(required_product(&product)?)?;
            let version = match version {
                Some(v) => v,
                None => vm.current_version()?.ok_or(OvmError::NoActiveVersion)?,
            };
            commands::info::run(&vm, &version)
        }
        Commands::Completions { shell } => {
            commands::completions::run(shell);
            Ok(())
        }
        Commands::AutoUpdate { first, second } => {
            commands::autoupdate::run(first.as_deref(), second.as_deref())
        }
        Commands::Shortcuts { yes } => commands::shortcuts::run(yes),
        Commands::SelfManage { command } => match command {
            SelfCommands::Update {
                channel,
                method,
                dry_run,
            } => commands::self_update::run(channel.as_deref(), &method, dry_run),
            SelfCommands::Channel { channel } => commands::self_manage::channel(channel.as_deref()),
            SelfCommands::Current => commands::self_manage::current(),
            SelfCommands::List => commands::self_manage::list(),
            SelfCommands::Use { version } => commands::self_manage::use_version(&version),
            SelfCommands::Rollback => commands::self_manage::rollback(),
            SelfCommands::RepairControl => commands::self_manage::repair_control(),
        },
        Commands::SelfUpdate {
            channel,
            method,
            dry_run,
        } => commands::self_update::run(channel.as_deref(), &method, dry_run),
        Commands::Doctor {
            product,
            version,
            fix,
        } => {
            let vm = VersionManager::new(required_product(&product)?)?;
            commands::doctor::run(&vm, version.as_deref(), fix)
        }
    };

    result
}

/// Claudex must keep its shared proxy-session lease across the initial
/// `claudex -> ovm` exec, but Claude and background helpers must not inherit
/// it. Restore close-on-exec at OVM's entrypoint while this process retains
/// the open descriptor for the duration of the real Claude session.
#[cfg(unix)]
fn claim_claudex_session_lease() -> Result<()> {
    const SESSION_LOCK_FD_ENV: &str = "OVM_CLAUDEX_SESSION_LOCK_FD";

    let Some(raw_fd) = std::env::var_os(SESSION_LOCK_FD_ENV) else {
        return Ok(());
    };
    std::env::remove_var(SESSION_LOCK_FD_ENV);
    let fd: i32 = raw_fd
        .to_string_lossy()
        .parse()
        .map_err(|_| OvmError::Message("Claudex passed an invalid proxy session lease".into()))?;
    if fd < 3 {
        return Err(OvmError::Message(
            "Claudex passed an invalid proxy session lease".into(),
        ));
    }

    // SAFETY: `fcntl` validates the inherited descriptor. We do not take
    // ownership; the process closes it automatically when the session exits.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: same validated descriptor; only FD_CLOEXEC is added.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(unix))]
fn claim_claudex_session_lease() -> Result<()> {
    Ok(())
}

/// Detect when ovm is invoked through a `~/.ovm/bin/<product>` launcher whose
/// symlink points at the ovm binary (multi-call dispatch). Returns the product
/// so the caller routes into that product's launch command, which runs
/// `maybe_auto_update`. Every managed product is wired this way in
/// [`crate::version_manager`].
fn invoked_as_product_launcher(argv0: Option<&String>) -> Option<Product> {
    let name = argv0
        .and_then(|value| Path::new(value).file_name())
        .and_then(|value| value.to_str())?;
    [Product::Claude, Product::Codex, Product::Pi]
        .into_iter()
        .find(|product| product.binary_name() == name)
}

fn run_yolo_launch(product: Product, args: &[String]) -> Result<()> {
    commands::launch::run(product, &yolo_launch_args(args))
}

/// `ovm cxf` / `ovm cxyf` → native Codex on the priority service tier,
/// injected as a `-c service_tier="priority"` config override (optionally
/// with yolo). The override wins over `~/.codex/config.toml`.
fn run_fast_codex(args: &[String], yolo: bool) -> Result<()> {
    let mut launch_args = vec!["-c".to_string(), "service_tier=\"priority\"".to_string()];
    launch_args.extend_from_slice(args);
    if yolo {
        launch_args = yolo_launch_args(&launch_args);
    }
    commands::launch::run(Product::Codex, &launch_args)
}

/// `ovm ccx[y][f]` → the ovm-claudex plugin (Claude Code on GPT-5.6).
/// `yolo` injects `--yolo`; `fast` injects `--fast` (priority service tier).
fn run_claudex(args: &[String], yolo: bool, fast: bool) -> Result<()> {
    let Some(plugin_path) = plugins::find_bundled("claudex") else {
        return Err(OvmError::Message(
            "claudex plugin not found — the ovm-claudex binary must be on your PATH.".into(),
        ));
    };
    let args = claudex_plugin_args(args, yolo, fast);
    let status = std::process::Command::new(plugin_path)
        .args(&args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()?;
    std::process::exit(status.code().unwrap_or(1));
}

/// The argument list handed to the `ovm-claudex` plugin for `ovm ccx[y][f]`.
/// `yolo` APPENDS `--yolo` (claudex forwards it to `ovm ccy`); `fast` PREPENDS
/// `--fast` (claudex consumes it to select the priority-tier model aliases).
/// The two stack: `ccxyf` yields `--fast … --yolo`.
fn claudex_plugin_args(args: &[String], yolo: bool, fast: bool) -> Vec<String> {
    let mut args = if yolo {
        yolo_launch_args(args)
    } else {
        args.to_vec()
    };
    if fast {
        args.insert(0, "--fast".to_string());
    }
    args
}

fn yolo_launch_args(args: &[String]) -> Vec<String> {
    let mut launch_args = Vec::with_capacity(args.len() + 1);
    launch_args.extend_from_slice(args);
    launch_args.push("--yolo".to_string());
    launch_args
}

fn resolve_install_request(
    product: &str,
    version: &str,
    npm: bool,
    dev: Option<String>,
    binary: Option<PathBuf>,
    bundle: Option<PathBuf>,
    link: bool,
) -> Result<(Product, InstallRequest)> {
    if binary.is_some() || bundle.is_some() || link || dev.is_some() {
        let Some(label) = dev else {
            return Err(OvmError::Message(
                "Use --dev <label> together with --binary or --bundle for local installs.".into(),
            ));
        };
        let product = required_product(product)?;
        if version != "dev" {
            return Err(OvmError::Message(
                "Dev installs use `ovm install codex dev --dev <label> --binary <path>`.".into(),
            ));
        }
        let source = match (binary, bundle) {
            (Some(path), None) => DevInstallSource::Binary(path),
            (None, Some(path)) => DevInstallSource::Bundle(path),
            (Some(_), Some(_)) => {
                return Err(OvmError::Message(
                    "Choose exactly one of --binary or --bundle for dev installs.".into(),
                ))
            }
            (None, None) => {
                return Err(OvmError::Message(
                    "Provide --binary or --bundle for a dev install.".into(),
                ))
            }
        };

        return Ok((
            product,
            InstallRequest::Dev {
                label,
                source,
                link,
            },
        ));
    }

    let product = required_product(product)?;
    Ok((
        product,
        InstallRequest::Standard {
            use_npm: npm,
            version: version.to_string(),
        },
    ))
}

fn is_builtin_command(cmd: &str) -> bool {
    matches!(
        cmd,
        "install"
            | "use"
            | "adopt"
            | "uninstall"
            | "list"
            | "ls"
            | "current"
            | "which"
            | "clean"
            | "cleanup"
            | "archive"
            | "select"
            | "switch"
            | "stats"
            | "shortcuts"
            | "autoupdate"
            | "auto-update"
            | "self"
            | "self-update"
            | "selfupdate"
            | "info"
            | "completions"
            | "help"
            | "__refresh-cache"
            | "claude"
            | "cc"
            | "ccy"
            | "ccx"
            | "ccxy"
            | "ccxf"
            | "ccxyf"
            | "codex"
            | "cx"
            | "cxy"
            | "cxf"
            | "cxyf"
            | "pi"
            | "-h"
            | "--help"
            | "-V"
            | "--version"
    )
}

fn required_product(value: &str) -> Result<Product> {
    Product::parse(value).ok_or_else(|| {
        OvmError::Message(format!(
            "Unknown product {value}. Use one of: claude, cc, codex, cx, pi."
        ))
    })
}

/// Translate clap parse failures into our `OvmError` so they go through the same
/// mochi-sad-face renderer as runtime errors. Adds friendly suggestions for the
/// common cases (missing positional, unknown subcommand) and falls back to the
/// raw clap message otherwise. Help/version requests are passed straight to
/// stdout and exit cleanly.
fn format_clap_error(err: clap::Error) -> OvmError {
    use clap::error::ErrorKind;

    if matches!(
        err.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        let _ = err.print();
        std::process::exit(0);
    }

    let context =
        err.context()
            .filter_map(|(kind, value)| match kind {
                clap::error::ContextKind::InvalidArg
                | clap::error::ContextKind::InvalidSubcommand => Some(value.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(", ");

    let hint = match err.kind() {
        ErrorKind::MissingRequiredArgument if context.contains("PRODUCT") => {
            "Tip: pass a product name (claude, codex, pi) or its alias (cc, cx). Some commands accept no product to show all — try `ovm current` or `ovm which`."
        }
        ErrorKind::MissingRequiredArgument if context.contains("VERSION") => {
            "Tip: pass a version, or `latest` to install the newest upstream release. List options with `ovm ls <product> --remote`."
        }
        ErrorKind::MissingRequiredArgument => {
            "Tip: see `ovm help <command>` for the full signature and examples."
        }
        ErrorKind::UnknownArgument | ErrorKind::InvalidSubcommand => {
            "Tip: see `ovm help` for the list of commands. Plugins of the form `ovm-<name>` are auto-discovered from PATH."
        }
        ErrorKind::InvalidValue => {
            "Tip: check the allowed values with `ovm help <command>`."
        }
        _ => "Tip: try `ovm help` or `ovm help <command>` for usage.",
    };

    let raw = err
        .to_string()
        .lines()
        .take_while(|line| !line.starts_with("Usage:"))
        .map(|line| line.trim_start_matches("error: "))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    OvmError::Message(format!("{raw}\n\n{hint}"))
}

fn abort(error: impl std::fmt::Display) -> ! {
    eprintln!();
    for line in mochi::SAD.lines() {
        eprintln!("{}", mochi::face_style(line));
    }
    eprintln!();
    eprintln!("{} {}", console::style("Error:").red().bold(), error);
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::{
        claim_claudex_session_lease, claudex_plugin_args, invoked_as_product_launcher,
        required_product, resolve_install_request, yolo_launch_args,
    };
    use crate::product::Product;
    use crate::version_manager::{DevInstallSource, InstallRequest};
    use std::path::{Path, PathBuf};

    #[test]
    #[cfg(unix)]
    fn claudex_session_lease_stays_with_ovm_but_not_background_descendants() {
        use fs4::FileExt;
        use std::fs::OpenOptions;
        use std::os::fd::AsRawFd;
        use std::process::Command;

        const TEST_PHASE_ENV: &str = "OVM_CLAUDEX_SESSION_LEASE_TEST_PHASE";
        const TEST_LOCK_FILE_ENV: &str = "OVM_CLAUDEX_SESSION_LEASE_TEST_LOCK_FILE";
        const TEST_PID_FILE_ENV: &str = "OVM_CLAUDEX_SESSION_LEASE_TEST_PID_FILE";

        match std::env::var(TEST_PHASE_ENV).as_deref() {
            Ok("handoff") => {
                use std::os::unix::process::CommandExt;

                let lock_path = std::env::var_os(TEST_LOCK_FILE_ENV)
                    .map(PathBuf::from)
                    .expect("session lock path");
                let lease = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&lock_path)
                    .expect("lease file");
                FileExt::lock_shared(&lease).expect("shared lease");
                let fd = lease.as_raw_fd();

                // Simulate claudex clearing close-on-exec for its one exec.
                // SAFETY: `fd` belongs to `lease` and remains open through exec.
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                assert!(flags >= 0);
                // SAFETY: same valid descriptor; only FD_CLOEXEC is cleared.
                assert_eq!(
                    unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) },
                    0
                );

                let error = Command::new(std::env::current_exe().expect("test binary"))
                    .args([
                        "--exact",
                        "tests::claudex_session_lease_stays_with_ovm_but_not_background_descendants",
                    ])
                    .env("OVM_CLAUDEX_SESSION_LOCK_FD", fd.to_string())
                    .env(TEST_PHASE_ENV, "owner")
                    .exec();
                panic!("handoff exec failed: {error}");
            }
            Ok("owner") => {
                claim_claudex_session_lease().expect("OVM claims lease");
                assert!(std::env::var_os("OVM_CLAUDEX_SESSION_LOCK_FD").is_none());
                let lock_path = std::env::var_os(TEST_LOCK_FILE_ENV)
                    .map(PathBuf::from)
                    .expect("session lock path");
                let contender = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(lock_path)
                    .expect("contender");
                assert!(
                    FileExt::try_lock(&contender).is_err(),
                    "OVM must retain the inherited lease"
                );

                let grandchild_pid_file = std::env::var_os(TEST_PID_FILE_ENV)
                    .map(PathBuf::from)
                    .expect("grandchild pid path");
                let status = Command::new("/bin/sh")
                    .args(["-c", "sleep 5 & echo $! > \"$1\"", "sh"])
                    .arg(&grandchild_pid_file)
                    .status()
                    .expect("spawn helper");
                assert!(status.success());
                return;
            }
            _ => {}
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let lock_path = temp.path().join("sessions.lock");
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .expect("lease file");

        // The isolated handoff process owns and exposes the descriptor, so no
        // parallel thread in this parent test runner can accidentally inherit
        // it. It execs into the OVM owner, which daemonizes a grandchild.
        let grandchild_pid_file = temp.path().join("grandchild.pid");
        let status = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tests::claudex_session_lease_stays_with_ovm_but_not_background_descendants",
            ])
            .env(TEST_PHASE_ENV, "handoff")
            .env(TEST_LOCK_FILE_ENV, &lock_path)
            .env(TEST_PID_FILE_ENV, &grandchild_pid_file)
            .status()
            .expect("spawn isolated OVM owner");
        assert!(status.success(), "isolated OVM owner failed");
        let grandchild_pid: i32 = std::fs::read_to_string(&grandchild_pid_file)
            .expect("grandchild pid")
            .trim()
            .parse()
            .expect("numeric grandchild pid");

        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("contender");
        FileExt::try_lock(&contender).expect("background grandchild must not retain the lease");
        // SAFETY: the PID came from the child we just spawned; failure merely
        // means it exited before cleanup.
        unsafe {
            libc::kill(grandchild_pid, libc::SIGTERM);
        }
    }

    #[test]
    fn requires_explicit_known_product() {
        assert_eq!(required_product("claude").expect("claude"), Product::Claude);
        assert_eq!(required_product("cx").expect("codex alias"), Product::Codex);
        assert!(required_product("latest").is_err());
    }

    #[test]
    fn yolo_launch_keeps_version_selector_first() {
        let args = vec!["latest".to_string()];
        assert_eq!(
            yolo_launch_args(&args),
            vec!["latest".to_string(), "--yolo".to_string()]
        );
    }

    #[test]
    fn claudex_plugin_args_inject_fast_and_yolo_for_each_alias() {
        let base = vec!["help".to_string()];
        // ccx: plain passthrough.
        assert_eq!(claudex_plugin_args(&base, false, false), vec!["help"]);
        // ccxy: --yolo appended (stays after the subcommand).
        assert_eq!(
            claudex_plugin_args(&base, true, false),
            vec!["help", "--yolo"]
        );
        // ccxf: --fast prepended (claudex consumes it to pick fast aliases).
        assert_eq!(
            claudex_plugin_args(&base, false, true),
            vec!["--fast", "help"]
        );
        // ccxyf: both stack.
        assert_eq!(
            claudex_plugin_args(&base, true, true),
            vec!["--fast", "help", "--yolo"]
        );
    }

    #[test]
    fn product_launcher_routes_every_managed_product() {
        let arg = |value: &str| Some(value.to_string());
        // bin/<product> is a symlink to the ovm binary, so launching it routes
        // through ovm's dispatch (and runs maybe_auto_update). Matched by the
        // launcher's file name, so a full path resolves the same.
        assert_eq!(
            invoked_as_product_launcher(arg("/home/u/.ovm/bin/codex").as_ref()),
            Some(Product::Codex)
        );
        assert_eq!(
            invoked_as_product_launcher(arg("pi").as_ref()),
            Some(Product::Pi)
        );
        assert_eq!(
            invoked_as_product_launcher(arg("claude").as_ref()),
            Some(Product::Claude)
        );
        // The ovm binary invoked as itself is not a product launcher.
        assert_eq!(invoked_as_product_launcher(arg("ovm").as_ref()), None);
        assert_eq!(invoked_as_product_launcher(None), None);
    }

    #[test]
    fn resolve_install_request_uses_explicit_product_and_version() {
        let (product, request) =
            resolve_install_request("claude", "latest", false, None, None, None, false)
                .expect("install request");

        assert_eq!(product, Product::Claude);
        match request {
            InstallRequest::Standard { use_npm, version } => {
                assert!(!use_npm);
                assert_eq!(version, "latest");
            }
            InstallRequest::Dev { .. } => panic!("expected standard install"),
        }
    }

    #[test]
    fn dev_install_requires_dev_placeholder_version() {
        let error = match resolve_install_request(
            "codex",
            "latest",
            false,
            Some("resume-fix".into()),
            Some(PathBuf::from("/tmp/codex")),
            None,
            false,
        ) {
            Ok(_) => panic!("expected invalid dev install"),
            Err(error) => error,
        };

        assert!(error
            .to_string()
            .contains("ovm install codex dev --dev <label> --binary <path>"));
    }

    #[test]
    fn dev_install_accepts_explicit_product_and_dev_placeholder() {
        let (product, request) = resolve_install_request(
            "codex",
            "dev",
            false,
            Some("resume-fix".into()),
            Some(PathBuf::from("/tmp/codex")),
            None,
            true,
        )
        .expect("dev install request");

        assert_eq!(product, Product::Codex);
        match request {
            InstallRequest::Dev {
                label,
                source,
                link,
            } => {
                assert_eq!(label, "resume-fix");
                assert!(
                    matches!(source, DevInstallSource::Binary(path) if path == Path::new("/tmp/codex"))
                );
                assert!(link);
            }
            InstallRequest::Standard { .. } => panic!("expected dev install"),
        }
    }
}
