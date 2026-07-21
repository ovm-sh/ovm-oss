use crate::commands::{maintain_claude_launcher, nudge_if_claude_install_drift};
use crate::config::OvmDirs;
use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::version_manager::{InstallRequest, VersionManager};
use console::style;
use std::path::{Path, PathBuf};
use std::process::Command;

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

    let output = version_output(&binary)?;
    let raw_version = extract_semver(&output).ok_or_else(|| {
        OvmError::Message(format!(
            "Could not parse a version from `{}` output:\n{}",
            binary.display(),
            output.trim()
        ))
    })?;
    let version = product.normalize_version(&raw_version);

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
        println!(
            "{} Installing managed {} {}",
            style("→").dim(),
            product.display_name(),
            style(&version).green().bold()
        );
        vm.install(InstallRequest::Standard {
            use_npm: false,
            version,
        })?
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
    if report_path_takeover(vm) {
        report_cleanup_hint(product, &binary);
    } else {
        eprintln!(
            "    Keep the original install until `{}` resolves to OVM.",
            product.binary_name()
        );
    }
    nudge_if_claude_install_drift(vm);

    Ok(())
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

fn find_foreign_binary_in_paths(
    dirs: &OvmDirs,
    product: Product,
    paths: &[PathBuf],
) -> Option<PathBuf> {
    paths
        .iter()
        .map(|dir| dir.join(product.binary_name()))
        .find(|candidate| candidate.is_file() && !is_ovm_managed(dirs, candidate))
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

fn report_cleanup_hint(product: Product, binary: &Path) {
    match cleanup_hint_for(product, binary) {
        Some(hint) => {
            println!(
                "  {} You can now remove the old {} install if you no longer want a fallback:",
                style("·").dim(),
                hint.manager
            );
            println!("    {}", style(hint.command).cyan());
        }
        None => {
            println!(
                "  {} You can now remove the old install manually if you no longer want a fallback.",
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
    let base = canonicalize_best_effort(&dirs.base);
    let candidate = canonicalize_best_effort(candidate);
    candidate.starts_with(base)
}

fn canonicalize_best_effort(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn version_output(binary: &Path) -> Result<String> {
    let output = Command::new(binary).arg("--version").output()?;
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
    use super::{cleanup_hint_for, extract_semver, find_foreign_binary_in_paths, CleanupHint};
    use crate::config::OvmDirs;
    use crate::product::Product;
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
