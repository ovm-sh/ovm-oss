//! `ovm stats` — show installed/archived counts and disk usage per product.

use crate::config::{OvmDirs, VersionSource};
use crate::error::Result;
use crate::product::Product;
use crate::version_manager::VersionManager;
use console::style;
use std::fs;
use std::path::Path;
use std::time::SystemTime;

pub fn run() -> Result<()> {
    println!();
    println!("  {}", style("OVM stats").bold());
    println!();

    let mut grand_total_bytes: u64 = 0;

    for product in Product::ALL {
        let vm = match VersionManager::new(product) {
            Ok(vm) => vm,
            Err(_) => continue,
        };

        let installed_names = vm.list_installed().unwrap_or_default();

        let mut installed = 0usize;
        let mut archived = 0usize;
        let mut total_bytes: u64 = 0;

        for name in &installed_names {
            let sources = vm.version_sources(name);
            if sources.contains(&VersionSource::Archived) && sources.len() == 1 {
                archived += 1;
            } else {
                installed += 1;
            }
            total_bytes += dir_size(&vm.product_dirs.version_dir(name)).unwrap_or(0);
        }
        grand_total_bytes += total_bytes;

        let active = vm.current_version().ok().flatten();
        let last_used = active
            .as_deref()
            .and_then(|v| last_used_time(&vm.product_dirs.version_dir(v)));

        println!("  {}", style(product.display_name()).bold());
        println!(
            "    installed: {}   archived: {}",
            style(installed).green(),
            if archived > 0 {
                style(archived).yellow().to_string()
            } else {
                style(archived).dim().to_string()
            }
        );
        match active {
            Some(v) => println!("    active:    {}", style(v).green().bold()),
            None => println!("    active:    {}", style("none").dim()),
        }
        println!("    size:      {}", format_bytes(total_bytes));
        if let Some(t) = last_used {
            println!("    last used: {}", style(humanize_age(t)).dim());
        }
        println!();
    }

    let dirs = OvmDirs::new()?;
    println!(
        "  {}  {}",
        style("Total disk usage:").bold(),
        format_bytes(grand_total_bytes)
    );
    println!(
        "  {}  {}",
        style("Storage root:").dim(),
        style(dirs.base.display()).dim()
    );
    println!();
    Ok(())
}

fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    if !path.exists() {
        return Ok(0);
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_file() {
            total += meta.len();
        } else if meta.is_dir() {
            total += dir_size(&entry.path()).unwrap_or(0);
        }
    }
    Ok(total)
}

fn last_used_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

fn humanize_age(t: SystemTime) -> String {
    let elapsed = match t.elapsed() {
        Ok(d) => d.as_secs(),
        Err(_) => return "just now".into(),
    };
    if elapsed < 60 {
        "just now".into()
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h ago", elapsed / 3600)
    } else if elapsed < 86_400 * 30 {
        format!("{}d ago", elapsed / 86_400)
    } else if elapsed < 86_400 * 365 {
        format!("{}mo ago", elapsed / (86_400 * 30))
    } else {
        format!("{}y ago", elapsed / (86_400 * 365))
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}
