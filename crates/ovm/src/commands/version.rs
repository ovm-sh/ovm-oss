//! `ovm version` — what am I running, and what is it managing?
//!
//! `--version` answers only for the binary; `ovm current` prints the active
//! version per product. Neither answers the question people actually type this
//! for: "what have I got?" This does, in one screen and without the network —
//! OVM itself plus every managed product, with the detail that changes what you
//! would do next (a pin holds you back, a dev build has no upstream at all).

use crate::error::Result;
use crate::product::Product;
use crate::version_manager::VersionManager;
use console::style;

/// One product's line, decided before anything is printed so the shape is
/// testable without a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Line {
    /// Active version, how many are installed, and why it might not move.
    Active {
        version: String,
        installed: usize,
        note: Option<String>,
    },
    /// Versions on disk but none selected — a real state, not an absence.
    InstalledNotSelected {
        installed: usize,
    },
    NotInstalled,
    /// The store could not be read. Reported, never rendered as "not installed".
    Unavailable(String),
}

/// The pure decision for one product.
pub(crate) fn line_for(active: Option<&str>, installed: usize, pin: Option<&str>) -> Line {
    match active {
        Some(version) => {
            // Both notes answer "why won't this move when I update?" — the one
            // question a version list is usually consulted for.
            let note = if version.starts_with("dev:") {
                Some("local build".to_string())
            } else if pin == Some(version) {
                Some("pinned".to_string())
            } else {
                None
            };
            Line::Active {
                version: version.to_string(),
                installed,
                note,
            }
        }
        None if installed > 0 => Line::InstalledNotSelected { installed },
        None => Line::NotInstalled,
    }
}

pub fn run() -> Result<()> {
    println!();
    println!(
        "  {} {}",
        style("ovm").bold(),
        style(env!("CARGO_PKG_VERSION")).green().bold()
    );
    println!();

    for product in Product::ALL {
        let line = match VersionManager::new(product) {
            Ok(vm) => {
                let active = vm.current_version().ok().flatten();
                let installed = vm.list_installed().map(|v| v.len()).unwrap_or(0);
                line_for(active.as_deref(), installed, vm.read_pin().as_deref())
            }
            Err(error) => Line::Unavailable(error.to_string()),
        };
        print_line(product, &line);
    }

    println!();
    println!(
        "  {}",
        style("ovm list <product> for every installed version").dim()
    );
    Ok(())
}

fn print_line(product: Product, line: &Line) {
    let name = format!("{:<9}", product.canonical_name());
    match line {
        Line::Active {
            version,
            installed,
            note,
        } => {
            let mut detail = format!("{installed} installed");
            if let Some(note) = note {
                detail = format!("{detail} · {note}");
            }
            println!(
                "  {name} {:<22} {}",
                style(version).green().bold(),
                style(detail).dim()
            );
        }
        Line::InstalledNotSelected { installed } => println!(
            "  {name} {:<22} {}",
            style("none selected").yellow(),
            style(format!(
                "{installed} installed · ovm use {} <version>",
                product.canonical_name()
            ))
            .dim()
        ),
        Line::NotInstalled => println!(
            "  {name} {:<22} {}",
            style("—").dim(),
            style(format!(
                "not installed · ovm install {} latest",
                product.canonical_name()
            ))
            .dim()
        ),
        Line::Unavailable(error) => println!(
            "  {name} {:<22} {}",
            style("unavailable").red(),
            style(error).dim()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_active_version_reports_how_many_are_installed() {
        assert_eq!(
            line_for(Some("2.1.221"), 5, None),
            Line::Active {
                version: "2.1.221".to_string(),
                installed: 5,
                note: None,
            }
        );
    }

    #[test]
    fn a_pin_is_called_out_because_it_stops_updates() {
        let line = line_for(Some("2.1.220"), 3, Some("2.1.220"));
        assert_eq!(
            line,
            Line::Active {
                version: "2.1.220".to_string(),
                installed: 3,
                note: Some("pinned".to_string()),
            }
        );
    }

    #[test]
    fn a_pin_on_a_different_version_is_not_holding_this_one() {
        let line = line_for(Some("2.1.221"), 3, Some("2.1.220"));
        assert!(matches!(line, Line::Active { note: None, .. }));
    }

    #[test]
    fn a_dev_build_says_so_because_it_has_no_upstream() {
        let line = line_for(Some("dev:local"), 1, None);
        assert!(matches!(
            line,
            Line::Active { ref note, .. } if note.as_deref() == Some("local build")
        ));
    }

    #[test]
    fn installed_but_unselected_is_distinct_from_not_installed() {
        assert_eq!(
            line_for(None, 4, None),
            Line::InstalledNotSelected { installed: 4 }
        );
        assert_eq!(line_for(None, 0, None), Line::NotInstalled);
    }
}
