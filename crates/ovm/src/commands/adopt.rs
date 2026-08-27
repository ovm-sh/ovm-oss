use crate::commands::{maintain_claude_launcher, nudge_if_claude_install_drift};
use crate::config::OvmDirs;
use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::version_manager::{InstallRequest, VersionManager};
use console::style;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Bring an existing unmanaged install under OVM without deleting it.
///
/// Adoption preserves the version the machine is already on: read it from the
/// binary, then make that exact version a managed install. Where the managed
/// layout is a single self-contained executable, the user's own binary is
/// **imported from disk** — no download, so this works with no network at all.
/// A wrapper script (npm/Homebrew shim) or a bundled product cannot be copied
/// as one file, so those fall back to fetching that same version from upstream
/// (see [`import_local_binary`]).
pub fn run(vm: &VersionManager, path: Option<PathBuf>) -> Result<()> {
    let product = vm.product();
    let binary = match path {
        Some(path) => path,
        None => find_foreign_binary(&vm.dirs, product)?,
    };

    if !binary.is_file() {
        return Err(OvmError::Message(format!(
            "{} binary not found at {}",
            product.display_name(),
            binary.display()
        )));
    }
    reject_ovm_managed_binary(vm, &binary)?;

    let version = reported_store_version(product, &binary)?;

    println!(
        "{} Found {} {} at {}",
        style("→").dim(),
        product.display_name(),
        style(&version).green().bold(),
        style(binary.display()).dim()
    );

    let installed_version = if vm.install_is_complete(&version) {
        println!(
            "{} {} {} already installed in OVM",
            style("✓").green(),
            product.display_name(),
            style(&version).green().bold()
        );
        version
    } else {
        match import_rejection(vm, &binary, &version) {
            None => {
                let installed = vm.install(InstallRequest::Import {
                    version,
                    binary: binary.clone(),
                })?;
                println!(
                    "{} Imported the local binary as managed {} {} — nothing downloaded",
                    style("✓").green(),
                    product.display_name(),
                    style(&installed).green().bold()
                );
                installed
            }
            Some(reason) => {
                println!(
                    "{} The local {} is {} — downloading managed {} {} instead",
                    style("→").dim(),
                    product.binary_name(),
                    reason,
                    product.display_name(),
                    style(&version).green().bold()
                );
                vm.install(InstallRequest::Standard {
                    use_npm: false,
                    version,
                })?
            }
        }
    };

    vm.use_version(&installed_version)?;
    maintain_claude_launcher(vm);

    println!(
        "{} Now using managed {} {}",
        style("✓").green(),
        product.display_name(),
        style(&installed_version).green().bold()
    );
    println!(
        "  {} Original install left untouched: {}",
        style("·").dim(),
        style(binary.display()).dim()
    );
    let path_taken_over = report_path_takeover(vm);
    report_cleanup_hint(product, &binary, path_taken_over);
    if !path_taken_over {
        eprintln!(
            "    Keep the original install until `{}` resolves to OVM.",
            product.binary_name()
        );
    }
    nudge_if_claude_install_drift(vm);

    Ok(())
}

/// Refuse a path that is already inside OVM's own store.
///
/// Adoption imports an install from *outside* OVM, and pointing it at a managed
/// path is not merely a redundant no-op. The import transaction quarantines and
/// removes that version's source tree before it copies anything, so a source
/// under the tree it is about to rebuild — an install that died half-way, say,
/// leaving a binary and no `.complete` — is deleted while it is still needed:
/// the copy fails and the file is gone, from the one command that promises to
/// leave the original untouched.
///
/// PATH discovery already filters these out ([`find_foreign_binary_in_paths`]);
/// this covers the path the user typed, which is the only way one gets in.
/// The message says what to run instead, because the user's real intent is
/// visible from the state on disk: an incomplete managed install wants
/// repairing, a complete one wants selecting.
fn reject_ovm_managed_binary(vm: &VersionManager, binary: &Path) -> Result<()> {
    if !is_ovm_managed(&vm.dirs, binary) {
        return Ok(());
    }

    let product = vm.product();
    Err(OvmError::Message(format!(
        "{} is already inside OVM's store ({}), so there is nothing to adopt.\n  \
         `ovm adopt` brings an install from OUTSIDE OVM under management.\n  \
         See what is installed:        `ovm ls {name}`\n  \
         Repair an incomplete install: `ovm install {name} <version>`\n  \
         Select a complete one:        `ovm use {name} <version>`",
        binary.display(),
        vm.dirs.base.display(),
        name = product.canonical_name(),
    )))
}

/// Decide whether the user's binary can become the managed install itself.
/// `None` means it can; `Some(reason)` is the user-facing explanation, phrased
/// to slot into "The local <binary> is <reason> — downloading … instead".
///
/// Adoption's whole point is that the version already on the machine is
/// preserved; downloading a byte-identical copy of it is a redundant transfer
/// on a good network and a hard failure on a bad one. So where the managed
/// layout for a product is one self-contained executable, copy theirs into the
/// store instead of fetching it (see
/// [`VersionManager::install`] with [`InstallRequest::Import`], which publishes
/// it under the same locked transaction a download install uses).
///
/// Rejected cases fall back to the download, because an import there would
/// produce an install that only *looks* managed:
///   - a wrapper script (`#!…`): an npm or Homebrew shim is a few lines that
///     reach into a package tree we are not copying, so the imported copy would
///     break the moment the user removes the original — which adopt goes on to
///     tell them they may do;
///   - Pi, whose managed install is a whole bundle (binary + `package.json` +
///     assets), not a single file;
///   - a version string OVM cannot map to a real release, which must never
///     become a directory name in the version store.
fn import_rejection(vm: &VersionManager, source: &Path, version: &str) -> Option<&'static str> {
    let product = vm.product();
    // `version` came from running a foreign binary, so it is untrusted input
    // that is about to become a path component and a store key.
    if vm.reject_version_traversal(version).is_err() || !product.is_official_remote_version(version)
    {
        return Some("reporting a version OVM cannot map to a release");
    }
    if !is_self_contained_executable(source) {
        return Some("a wrapper script, not a self-contained binary");
    }
    // Pi ships as a bundle; one copied file would be a broken install.
    if product.is_bundle() {
        return Some("part of a bundle OVM cannot copy as one file");
    }

    None
}

/// Whether `path` is an executable OVM can copy on its own — i.e. not a `#!`
/// script standing in for a package installed elsewhere.
fn is_self_contained_executable(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 2];
    match file.read_exact(&mut magic) {
        Ok(()) => &magic != b"#!",
        // Too short to be a real program; let the download decide.
        Err(_) => false,
    }
}

/// The unmanaged install of this product on PATH, if any.
///
/// Used by the first-launch bootstrap to prefer adopting what the machine
/// already has over downloading a fresh copy, and to fall back to executing it
/// when nothing can be installed. Answers `None` on any problem (no PATH,
/// nothing found) — the caller falls back to installing.
pub(crate) fn foreign_binary_on_path(dirs: &OvmDirs, product: Product) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let paths = std::env::split_paths(&path).collect::<Vec<_>>();
    find_foreign_binary_in_paths(dirs, product, &paths)
}

fn find_foreign_binary(dirs: &OvmDirs, product: Product) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").ok_or_else(|| {
        OvmError::Message(format!(
            "PATH is not set. Pass the existing binary path explicitly: `ovm adopt {} /path/to/{}`",
            product.canonical_name(),
            product.binary_name()
        ))
    })?;
    let paths = std::env::split_paths(&path).collect::<Vec<_>>();
    find_foreign_binary_in_paths(dirs, product, &paths).ok_or_else(|| {
        OvmError::Message(format!(
            "No non-OVM {} binary found on PATH. Pass one explicitly: `ovm adopt {} /path/to/{}`",
            product.binary_name(),
            product.canonical_name(),
            product.binary_name()
        ))
    })
}

pub(crate) fn find_foreign_binary_in_paths(
    dirs: &OvmDirs,
    product: Product,
    paths: &[PathBuf],
) -> Option<PathBuf> {
    paths
        .iter()
        .map(|dir| dir.join(product.binary_name()))
        .find(|candidate| {
            candidate.is_file()
                && !is_ovm_managed(dirs, candidate)
                && !is_session_shim(candidate)
                && !is_ovm_launcher(candidate)
        })
}

/// Whether `candidate` is a per-session wrapper shim another tool injected
/// into PATH, rather than an install of the product.
///
/// cmux writes a `claude`/`codex` shim under `$TMPDIR/cmux-cli-shims/<uuid>/`
/// for every terminal surface and prepends that directory to PATH, so its
/// wrapper can inject session tracking before delegating to the real binary
/// (typically ours). It is not an install: there is nothing to adopt, warning
/// that it "shadows the managed install" is a false alarm, and the bootstrap
/// exec fallback could re-enter it. Stale shim directories also outlive their
/// sessions by weeks, so the one found on PATH may not even be live.
fn is_session_shim(candidate: &Path) -> bool {
    candidate
        .components()
        .any(|component| component.as_os_str() == "cmux-cli-shims")
}

/// Whether `candidate` is OVM's own control-plane launcher — from ANY OVM
/// home, not just this process's. `is_ovm_managed` compares against the
/// running `$HOME/.ovm`, so a launcher under a different home (a sandboxed
/// `$HOME`, another user on a shared machine) read as a foreign install to
/// adopt.
/// Adopting one adopts a wrapper that re-enters OVM, and probing it under the
/// mismatched HOME re-runs OVM's own first-launch bootstrap — which is how a
/// sandboxed `ovm hatch` wedged mid-story on 2026-08-27, "adopting" the real
/// home's `.ovm/bin/claude`.
fn is_ovm_launcher(candidate: &Path) -> bool {
    let Some(parent) = candidate.parent() else {
        return false;
    };
    parent.file_name() == Some(std::ffi::OsStr::new("bin"))
        && parent.parent().and_then(|dir| dir.file_name()) == Some(std::ffi::OsStr::new(".ovm"))
}

fn report_path_takeover(vm: &VersionManager) -> bool {
    let product = vm.product();
    let Some(path) = std::env::var_os("PATH") else {
        warn_path_not_taken_over(
            product,
            &format!(
                "PATH is not set. Add {} before launching {}.",
                vm.dirs.bin.display(),
                product.binary_name()
            ),
        );
        return false;
    };
    let paths = std::env::split_paths(&path).collect::<Vec<_>>();
    let Some(first) = first_binary_in_paths(product, &paths) else {
        warn_path_not_taken_over(
            product,
            &format!(
                "PATH does not find `{}`. Add {} to PATH.",
                product.binary_name(),
                vm.dirs.bin.display()
            ),
        );
        return false;
    };

    if paths_refer_to_same_file(&first, &vm.product_dirs.active_bin) {
        println!(
            "  {} PATH now resolves `{}` to OVM: {}",
            style("✓").green(),
            product.binary_name(),
            style(first.display()).dim()
        );
        true
    } else {
        warn_path_not_taken_over(
            product,
            &format!(
                "`{}` still resolves to {} before OVM's {}",
                product.binary_name(),
                first.display(),
                vm.product_dirs.active_bin.display()
            ),
        );
        false
    }
}

fn first_binary_in_paths(product: Product, paths: &[PathBuf]) -> Option<PathBuf> {
    paths
        .iter()
        .map(|dir| dir.join(product.binary_name()))
        .find(|candidate| candidate.is_file())
}

fn warn_path_not_taken_over(product: Product, reason: &str) {
    eprintln!(
        "  {} Adopted, but PATH has not taken over for `{}`: {}",
        style("⚠").yellow(),
        product.binary_name(),
        reason
    );
    eprintln!("    Put OVM first: export PATH=\"$HOME/.ovm/bin:$PATH\"");
}

fn paths_refer_to_same_file(left: &Path, right: &Path) -> bool {
    canonicalize_best_effort(left) == canonicalize_best_effort(right)
}

#[derive(Debug, PartialEq, Eq)]
struct CleanupHint {
    manager: &'static str,
    command: String,
}

fn report_cleanup_hint(product: Product, binary: &Path, path_taken_over: bool) {
    let timing = if path_taken_over {
        "You can now"
    } else {
        "After PATH resolves to OVM, you can"
    };
    match cleanup_hint_for(product, binary) {
        Some(hint) => {
            println!(
                "  {} {timing} remove the old {} install if you no longer want a fallback:",
                style("·").dim(),
                hint.manager
            );
            println!("    {}", style(hint.command).cyan());
        }
        None => {
            println!(
                "  {} {timing} remove the old install manually if you no longer want a fallback.",
                style("·").dim()
            );
        }
    }
}

fn cleanup_hint_for(product: Product, binary: &Path) -> Option<CleanupHint> {
    let canonical = canonicalize_best_effort(binary);

    homebrew_cleanup_hint(&canonical)
        .or_else(|| npm_cleanup_hint(&canonical))
        .or_else(|| claude_native_cleanup_hint(product, binary, &canonical))
}

fn homebrew_cleanup_hint(path: &Path) -> Option<CleanupHint> {
    package_after_component(path, "Cellar")
        .map(|formula| CleanupHint {
            manager: "Homebrew",
            command: format!("brew uninstall {formula}"),
        })
        .or_else(|| {
            package_after_component(path, "Caskroom").map(|cask| CleanupHint {
                manager: "Homebrew cask",
                command: format!("brew uninstall --cask {cask}"),
            })
        })
}

fn npm_cleanup_hint(path: &Path) -> Option<CleanupHint> {
    npm_package_from_path(path).map(|package| CleanupHint {
        manager: "npm global",
        command: format!("npm uninstall -g {package}"),
    })
}

fn claude_native_cleanup_hint(
    product: Product,
    binary: &Path,
    canonical: &Path,
) -> Option<CleanupHint> {
    if product != Product::Claude {
        return None;
    }

    let home = dirs::home_dir()?;
    let local = home.join(".local");
    if binary.starts_with(&local) || canonical.starts_with(&local) {
        return Some(CleanupHint {
            manager: "Claude native",
            command: "ovm doctor claude --fix".into(),
        });
    }

    None
}

fn package_after_component(path: &Path, marker: &str) -> Option<String> {
    let mut components = path.components();
    while let Some(component) = components.next() {
        if component.as_os_str() == marker {
            return components
                .next()
                .map(|component| component.as_os_str().to_string_lossy().into_owned());
        }
    }
    None
}

fn npm_package_from_path(path: &Path) -> Option<String> {
    let parts = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    for (index, part) in parts.iter().enumerate() {
        if part != "node_modules" {
            continue;
        }

        let package = parts.get(index + 1)?;
        if package.starts_with('@') {
            let name = parts.get(index + 2)?;
            return Some(format!("{package}/{name}"));
        }

        return Some(package.clone());
    }

    None
}

fn is_ovm_managed(dirs: &OvmDirs, candidate: &Path) -> bool {
    // Walk the symlink chain and judge each hop by WHERE IT LIVES, never by
    // where the chain ultimately lands. Two of our own launchers used to
    // read as foreign installs under a Homebrew/cargo OVM (executable
    // outside ~/.ovm): the ~/.ovm/bin shim resolves to that executable, and
    // ~/.local/bin/claude resolves through the shim to the same place — so
    // full canonicalization left the base directory and the only remaining
    // test, current_exe, fails whenever the control plane exec'd a
    // self-managed version. A hop whose own directory canonicalizes into
    // ~/.ovm is ours by construction, whatever it points at.
    let mut hop = candidate.to_path_buf();
    for _ in 0..16 {
        let dir_is_ours = hop
            .parent()
            .is_some_and(|parent| crate::version_manager::path_is_inside(&dirs.base, parent));
        if dir_is_ours {
            return true;
        }
        match std::fs::read_link(&hop) {
            Ok(target) if target.is_absolute() => hop = target,
            Ok(target) => match hop.parent() {
                Some(parent) => hop = parent.join(target),
                None => break,
            },
            Err(_) => break,
        }
    }
    // A launcher whose chain never touches ~/.ovm but resolves to the
    // running OVM executable is ours too (a shim created before the base
    // moved, an exotic install layout).
    if let (Ok(resolved), Ok(own_exe)) = (
        candidate.canonicalize(),
        std::env::current_exe().and_then(|exe| exe.canonicalize()),
    ) {
        if resolved == own_exe {
            return true;
        }
    }
    false
}

fn canonicalize_best_effort(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Ask `binary` what it is, and answer with the version OVM would store it
/// under (`--version` output → semver → the product's storage spelling, e.g.
/// Codex's `rust-v0.144.0`).
///
/// One definition on purpose. Adoption decides *which* version it is adopting
/// with this, and the import install re-asks the copy it staged with the same
/// question ([`crate::version_manager`]); if the two derivations could drift,
/// the re-check would compare a version against a differently-spelled version
/// and reject binaries that are perfectly fine.
pub(crate) fn reported_store_version(product: Product, binary: &Path) -> Result<String> {
    let output = version_output(binary)?;
    let raw_version = extract_semver(&output).ok_or_else(|| {
        OvmError::Message(format!(
            "Could not parse a version from `{}` output:\n{}",
            binary.display(),
            output.trim()
        ))
    })?;
    Ok(product.normalize_version(&raw_version))
}

fn version_output(binary: &Path) -> Result<String> {
    // The caller often executes a binary written moments earlier (the staged
    // import copy). On Linux, a concurrently forked child — OVM's own detached
    // background refresh, or a parallel test's spawn — can still hold the
    // write descriptor across its fork-to-exec window, and executing the file
    // then fails with ETXTBSY. The condition clears as soon as that child
    // execs, so a short bounded retry is the whole fix; any persistent
    // ETXTBSY (a genuinely running binary) still surfaces.
    let output = {
        let mut attempt = 0;
        loop {
            match Command::new(binary).arg("--version").output() {
                Err(error)
                    if error.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 5 =>
                {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                other => break other,
            }
        }
    }?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = format!("{stdout}{stderr}");

    if !output.status.success() && extract_semver(&text).is_none() {
        return Err(OvmError::Message(format!(
            "`{} --version` failed with status {}:\n{}",
            binary.display(),
            output.status,
            text.trim()
        )));
    }

    Ok(text)
}

fn extract_semver(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    for start in 0..bytes.len() {
        if !bytes[start].is_ascii_digit() {
            continue;
        }

        let Some(end) = parse_semver_end(bytes, start) else {
            continue;
        };
        return Some(text[start..end].to_string());
    }

    None
}

fn parse_semver_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = consume_digits(bytes, start)?;

    if bytes.get(index) != Some(&b'.') {
        return None;
    }
    index += 1;
    index = consume_digits(bytes, index)?;

    if bytes.get(index) != Some(&b'.') {
        return None;
    }
    index += 1;
    index = consume_digits(bytes, index)?;

    if bytes.get(index) == Some(&b'-') {
        index += 1;
        let prerelease_start = index;
        while let Some(byte) = bytes.get(index) {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-') {
                index += 1;
            } else {
                break;
            }
        }
        if index == prerelease_start {
            return None;
        }
    }

    Some(index)
}

fn consume_digits(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    while let Some(byte) = bytes.get(index) {
        if byte.is_ascii_digit() {
            index += 1;
        } else {
            break;
        }
    }

    (index > start).then_some(index)
}

#[cfg(test)]
mod tests {
    use super::{
        cleanup_hint_for, extract_semver, find_foreign_binary_in_paths, import_rejection,
        CleanupHint,
    };
    use crate::config::{OvmConfig, OvmDirs};
    use crate::product::Product;
    use crate::version_manager::VersionManager;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    #[test]
    fn extracts_semver_from_tool_output() {
        assert_eq!(extract_semver("claude 2.1.91\n"), Some("2.1.91".into()));
        assert_eq!(
            extract_semver("codex-cli 0.135.0 (rust-v0.135.0)\n"),
            Some("0.135.0".into())
        );
        assert_eq!(
            extract_semver("pi v1.2.3-beta.1"),
            Some("1.2.3-beta.1".into())
        );
        assert_eq!(extract_semver("version unknown"), None);
    }

    #[test]
    fn path_search_skips_ovm_managed_binary() {
        let root = tempdir().expect("tempdir");
        let dirs = OvmDirs::at(root.path().join(".ovm"));
        let ovm_bin = dirs.bin.clone();
        let foreign_bin = root.path().join("usr-local-bin");
        fs::create_dir_all(&ovm_bin).expect("mkdir ovm bin");
        fs::create_dir_all(&foreign_bin).expect("mkdir foreign bin");
        fs::write(ovm_bin.join("codex"), "ovm").expect("write ovm bin");
        fs::write(foreign_bin.join("codex"), "foreign").expect("write foreign bin");

        let found =
            find_foreign_binary_in_paths(&dirs, Product::Codex, &[ovm_bin, foreign_bin.clone()])
                .expect("found foreign binary");

        assert_eq!(found, foreign_bin.join("codex"));
    }

    #[test]
    fn path_search_skips_cmux_session_shims() {
        // cmux prepends $TMPDIR/cmux-cli-shims/<uuid>/ to PATH in its terminal
        // surfaces; the claude/codex wrappers there are session plumbing, not
        // installs, and must never be reported (or adopted) as foreign.
        let root = tempdir().expect("tempdir");
        let dirs = OvmDirs::at(root.path().join(".ovm"));
        let shim_bin = root
            .path()
            .join("cmux-cli-shims")
            .join("71624E4F-3350-4EA4-A9C6-90D5D04982A9");
        let foreign_bin = root.path().join("usr-local-bin");
        fs::create_dir_all(&shim_bin).expect("mkdir shim bin");
        fs::create_dir_all(&foreign_bin).expect("mkdir foreign bin");
        fs::write(shim_bin.join("claude"), "#!/usr/bin/env bash\n").expect("write shim");
        fs::write(foreign_bin.join("claude"), "foreign").expect("write foreign bin");

        let found = find_foreign_binary_in_paths(
            &dirs,
            Product::Claude,
            &[shim_bin.clone(), foreign_bin.clone()],
        );
        assert_eq!(found, Some(foreign_bin.join("claude")));

        let only_shim = find_foreign_binary_in_paths(&dirs, Product::Claude, &[shim_bin]);
        assert_eq!(only_shim, None, "a session shim alone is not an install");
    }

    #[test]
    fn path_search_skips_another_homes_ovm_launcher() {
        // A sandboxed $HOME (or another user on the machine) makes this
        // process's OvmDirs point somewhere else, so the REAL home's
        // ~/.ovm/bin/claude passes `is_ovm_managed` and reads as a foreign
        // install. It is OVM's own launcher: adopting it adopts a wrapper
        // that re-enters OVM, and probing it re-runs our own first-launch
        // bootstrap under a HOME with no state — a hang, not an install.
        let root = tempdir().expect("tempdir");
        let dirs = OvmDirs::at(root.path().join("sandbox-home").join(".ovm"));
        let other_home_bin = root.path().join("real-home").join(".ovm").join("bin");
        let foreign_bin = root.path().join("usr-local-bin");
        fs::create_dir_all(&other_home_bin).expect("mkdir other home bin");
        fs::create_dir_all(&foreign_bin).expect("mkdir foreign bin");
        fs::write(other_home_bin.join("claude"), "ovm launcher").expect("write launcher");
        fs::write(foreign_bin.join("claude"), "foreign").expect("write foreign bin");

        let found = find_foreign_binary_in_paths(
            &dirs,
            Product::Claude,
            &[other_home_bin.clone(), foreign_bin.clone()],
        );
        assert_eq!(found, Some(foreign_bin.join("claude")));

        let only_launcher = find_foreign_binary_in_paths(&dirs, Product::Claude, &[other_home_bin]);
        assert_eq!(
            only_launcher, None,
            "OVM's own launcher from another home is not an install to adopt"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_ovm_shim_resolving_outside_the_base_is_still_ours() {
        // With a Homebrew or cargo OVM, ~/.ovm/bin/<product> is a symlink to
        // the ovm executable OUTSIDE ~/.ovm. Judging the resolved target
        // alone misread our own shim as an unmanaged install — the location
        // under ~/.ovm decides first.
        let root = tempdir().expect("tempdir");
        let dirs = OvmDirs::at(root.path().join(".ovm"));
        fs::create_dir_all(&dirs.bin).expect("mkdir ovm bin");
        let outside_exe = root.path().join("homebrew-bin").join("ovm");
        fs::create_dir_all(outside_exe.parent().expect("parent")).expect("mkdir");
        fs::write(&outside_exe, "the ovm executable").expect("write");
        let shim = dirs.bin.join("codex");
        std::os::unix::fs::symlink(&outside_exe, &shim).expect("shim");

        let found =
            find_foreign_binary_in_paths(&dirs, Product::Codex, std::slice::from_ref(&dirs.bin));

        assert_eq!(found, None, "our own shim must never read as foreign");
    }

    #[test]
    fn bundle_adoption_rejects_a_single_file_import() {
        let root = tempdir().expect("tempdir");
        let dirs = OvmDirs::at(root.path().join(".ovm"));
        let vm = VersionManager::with(dirs, OvmConfig::default(), Product::Pi);
        let binary = root.path().join("foreign-pi");
        fs::write(&binary, b"not-a-wrapper").expect("write foreign binary");

        assert_eq!(
            import_rejection(&vm, &binary, "0.79.10"),
            Some("part of a bundle OVM cannot copy as one file")
        );
    }

    #[test]
    fn cleanup_hint_detects_homebrew_formula() {
        let hint = cleanup_hint_for(
            Product::Codex,
            Path::new("/opt/homebrew/Cellar/codex/0.135.0/bin/codex"),
        );

        assert_eq!(
            hint,
            Some(CleanupHint {
                manager: "Homebrew",
                command: "brew uninstall codex".into()
            })
        );
    }

    #[test]
    fn cleanup_hint_detects_homebrew_cask() {
        let hint = cleanup_hint_for(
            Product::Claude,
            Path::new("/opt/homebrew/Caskroom/claude/2.1.91/claude"),
        );

        assert_eq!(
            hint,
            Some(CleanupHint {
                manager: "Homebrew cask",
                command: "brew uninstall --cask claude".into()
            })
        );
    }

    #[test]
    fn cleanup_hint_detects_scoped_npm_package() {
        let hint = cleanup_hint_for(
            Product::Claude,
            Path::new("/usr/local/lib/node_modules/@anthropic-ai/claude-code/cli.js"),
        );

        assert_eq!(
            hint,
            Some(CleanupHint {
                manager: "npm global",
                command: "npm uninstall -g @anthropic-ai/claude-code".into()
            })
        );
    }
}
