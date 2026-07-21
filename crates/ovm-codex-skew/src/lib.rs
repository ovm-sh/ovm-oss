//! Codex state-DB schema-skew guard.
//!
//! Codex keeps mutable state in a single, forward-migrated SQLite store every
//! installed version shares (`~/.codex/state_<n>.sqlite`). A newer Codex can
//! apply a **breaking** migration — one that removes a table/column an older
//! Codex still reads (e.g. migration 35 "drop memory tables", which moved the
//! memory `jobs` table out into a separate DB).
//!
//! Because OVM lets you run many versions — release pins *and* dev builds —
//! against that one store, running a newer version **once** migrates the DB and
//! silently degrades every older version: the old binary still opens the DB
//! (sqlx tolerates a newer-than-itself DB) but fails *soft* at runtime ("no such
//! table: jobs") and quietly drops functionality while appearing to work.
//!
//! The guard turns that silent failure into a loud, pre-flight signal with zero
//! source and zero network: `sqlx` compiles each migration's *description* into
//! the binary and stores the same description as TEXT in the DB's
//! `_sqlx_migrations` table. We byte-scan both — "migrations this binary knows"
//! vs "migrations the DB has applied" — and flag any applied migration the
//! binary doesn't understand, marking it breaking via a manifest generated from
//! Codex's open-source migration SQL.
//!
//! This crate is the single home for Codex schema-skew logic. When installed,
//! `ovm` invokes the `ovm-codex-skew` binary as Codex's optional companion at
//! lifecycle events (pre-launch, post-switch) and for `ovm doctor codex` — see
//! `crates/ovm/src/companions.rs`. Regenerate the manifest below with
//! `scripts/gen-codex-migration-manifest.py` when Codex adds migrations.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Migration {
    pub version: u32,
    pub description: &'static str,
    /// Removes a table/column an older binary still reads (a drop without a
    /// same-name recreate / rename). A rebuild that recreates the table under
    /// the same name is *not* breaking.
    pub breaking: bool,
}

// Codex `state` migrator — generated from openai/codex codex-rs/state/migrations
// (regenerate with scripts/gen-codex-migration-manifest.py). Keep in version
// order; `breaking` flags removals only.
#[rustfmt::skip]
const CODEX_STATE_MIGRATIONS: &[Migration] = &[
    Migration { version: 1, description: "threads", breaking: false },
    Migration { version: 2, description: "logs", breaking: false },
    Migration { version: 3, description: "logs thread id", breaking: false },
    Migration { version: 4, description: "thread dynamic tools", breaking: false },
    Migration { version: 5, description: "threads cli version", breaking: false },
    Migration { version: 6, description: "memories", breaking: false },
    Migration { version: 7, description: "threads first user message", breaking: false },
    Migration { version: 8, description: "backfill state", breaking: false },
    Migration { version: 9, description: "stage1 outputs rollout slug", breaking: false },
    Migration { version: 10, description: "logs process id", breaking: false },
    Migration { version: 11, description: "logs partition prune indexes", breaking: false },
    Migration { version: 12, description: "logs estimated bytes", breaking: false },
    Migration { version: 13, description: "threads agent nickname", breaking: false },
    Migration { version: 14, description: "agent jobs", breaking: false },
    Migration { version: 15, description: "agent jobs max runtime seconds", breaking: false },
    Migration { version: 16, description: "memory usage", breaking: false },
    Migration { version: 17, description: "phase2 selection flag", breaking: false },
    Migration { version: 18, description: "phase2 selection snapshot", breaking: false },
    Migration { version: 19, description: "thread dynamic tools defer loading", breaking: false },
    Migration { version: 20, description: "threads model reasoning effort", breaking: false },
    Migration { version: 21, description: "thread spawn edges", breaking: false },
    Migration { version: 22, description: "threads agent path", breaking: false },
    Migration { version: 23, description: "drop logs", breaking: true }, // removes logs
    Migration { version: 24, description: "remote control enrollments", breaking: false },
    Migration { version: 25, description: "thread timestamps millis", breaking: false },
    Migration { version: 26, description: "thread dynamic tools namespace", breaking: false },
    Migration { version: 27, description: "threads cwd sort indexes", breaking: false },
    Migration { version: 28, description: "device key bindings", breaking: false },
    Migration { version: 29, description: "thread goals", breaking: false },
    Migration { version: 30, description: "threads thread source", breaking: false },
    Migration { version: 31, description: "drop device key bindings", breaking: true }, // removes device_key_bindings
    Migration { version: 32, description: "threads preview", breaking: false },
    Migration { version: 33, description: "thread goal stopped statuses", breaking: false },
    Migration { version: 34, description: "drop thread goals", breaking: true }, // removes thread_goals
    Migration { version: 35, description: "drop memory tables", breaking: true }, // removes jobs, stage1_outputs
    Migration { version: 36, description: "threads visible sort indexes", breaking: false },
    Migration { version: 37, description: "remote control enrollments enabled", breaking: false },
    Migration { version: 38, description: "external agent config imports", breaking: false },
    Migration { version: 39, description: "threads recency at", breaking: false },
];

/// The verdict of comparing one binary against the on-disk state DB.
pub struct Assessment {
    pub state_db: PathBuf,
    pub db_max_applied: u32,
    pub binary_max_known: u32,
    /// Applied in the DB but unknown to this binary, in version order.
    pub ahead: Vec<&'static Migration>,
}

impl Assessment {
    /// True when the DB has a breaking migration this binary can't understand —
    /// i.e. running this binary against this DB risks degraded functionality.
    pub fn degraded(&self) -> bool {
        self.ahead.iter().any(|m| m.breaking)
    }

    pub fn breaking(&self) -> impl Iterator<Item = &'static Migration> + '_ {
        self.ahead.iter().copied().filter(|m| m.breaking)
    }

    pub fn additive(&self) -> impl Iterator<Item = &'static Migration> + '_ {
        self.ahead.iter().copied().filter(|m| !m.breaking)
    }
}

/// `~/.codex` — Codex's shared, forward-migrated state store.
fn state_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".codex"))
}

/// Newest `state_<n>.sqlite` in `dir` (highest numeric suffix is the live one).
fn newest_state_db(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_str()?;
        if let Some(n) = name
            .strip_prefix("state_")
            .and_then(|rest| rest.strip_suffix(".sqlite"))
            .and_then(|digits| digits.parse::<u32>().ok())
        {
            if best.as_ref().is_none_or(|(best_n, _)| n > *best_n) {
                best = Some((n, entry.path()));
            }
        }
    }
    best.map(|(_, path)| path)
}

/// Versions whose embedded/stored description bytes appear in any of `paths`.
/// sqlx writes descriptions verbatim into both the binary and the DB (and recent
/// inserts may sit in the `-wal` sidecar, so callers pass it too).
///
/// Single O(n) pass per file: descriptions are bucketed by first byte, so most
/// positions skip with one comparison. A naive `windows()` scan over a ~25 MB
/// binary took ~minutes; this runs in milliseconds.
fn versions_present(paths: &[PathBuf], manifest: &[Migration]) -> BTreeSet<u32> {
    let mut buckets: [Vec<&Migration>; 256] = std::array::from_fn(|_| Vec::new());
    let mut remaining = 0;
    for migration in manifest {
        if let Some(&first) = migration.description.as_bytes().first() {
            buckets[first as usize].push(migration);
            remaining += 1;
        }
    }

    let mut found = BTreeSet::new();
    'scan: for path in paths {
        let Ok(bytes) = fs::read(path) else { continue };
        for (i, &byte) in bytes.iter().enumerate() {
            let candidates = &mut buckets[byte as usize];
            let mut j = 0;
            while j < candidates.len() {
                if bytes[i..].starts_with(candidates[j].description.as_bytes()) {
                    found.insert(candidates[j].version);
                    candidates.swap_remove(j);
                    remaining -= 1;
                    if remaining == 0 {
                        break 'scan;
                    }
                } else {
                    j += 1;
                }
            }
        }
    }
    found
}

fn max_version(set: &BTreeSet<u32>) -> u32 {
    set.last().copied().unwrap_or(0)
}

/// Diff "applied in the DB" against "known to the binary" into an [`Assessment`].
fn build_assessment(
    state_db: PathBuf,
    applied: &BTreeSet<u32>,
    known: &BTreeSet<u32>,
    manifest: &'static [Migration],
) -> Assessment {
    Assessment {
        state_db,
        db_max_applied: max_version(applied),
        binary_max_known: max_version(known),
        ahead: manifest
            .iter()
            .filter(|m| applied.contains(&m.version) && !known.contains(&m.version))
            .collect(),
    }
}

/// Compare a Codex `binary` against the live `~/.codex` state DB. Returns `None`
/// when there's no state DB yet, or nothing has been applied (nothing to guard).
pub fn assess(binary: &Path) -> Option<Assessment> {
    let dir = state_dir()?;
    let state_db = newest_state_db(&dir)?;

    let wal = state_db.with_extension("sqlite-wal");
    let applied = versions_present(&[state_db.clone(), wal], CODEX_STATE_MIGRATIONS);
    if applied.is_empty() {
        return None;
    }
    let known = versions_present(&[binary.to_path_buf()], CODEX_STATE_MIGRATIONS);

    Some(build_assessment(
        state_db,
        &applied,
        &known,
        CODEX_STATE_MIGRATIONS,
    ))
}

/// Indented bullet lines for the breaking migrations in `assessment`.
pub fn breaking_bullets(assessment: &Assessment) -> Vec<String> {
    assessment
        .breaking()
        .map(|m| {
            format!(
                "       {} {} — {}",
                console::style("·").yellow(),
                m.version,
                m.description
            )
        })
        .collect()
}

/// Detailed report for `ovm doctor codex` — printed to stdout. `binary` is the
/// assessed Codex binary; `assessment` is `None` when there's no state DB yet.
pub fn print_report(version: &str, binary: &Path, assessment: Option<&Assessment>) {
    let label = if version.is_empty() {
        "active"
    } else {
        version
    };
    println!("schema-guard · Codex {label}");
    println!("  binary   : {}", binary.display());

    let Some(assessment) = assessment else {
        println!(
            "  {} no Codex state DB found yet — nothing to check.",
            console::style("✓").green()
        );
        return;
    };

    println!(
        "  state db : {} (applied up to migration {})",
        assessment.state_db.display(),
        assessment.db_max_applied
    );
    println!(
        "  this build knows migrations up to {}",
        assessment.binary_max_known
    );

    if assessment.ahead.is_empty() {
        println!(
            "  {} in sync — this build knows every applied migration.",
            console::style("✓").green()
        );
        return;
    }

    let additive = assessment.additive().count();
    if additive > 0 {
        println!(
            "  {} {additive} additive migration(s) newer than this build (forward-compatible)",
            console::style("·").dim()
        );
    }

    if assessment.degraded() {
        println!(
            "  {}  DEGRADE RISK — the DB was migrated by a newer Codex with breaking",
            console::style("⚠").yellow()
        );
        println!("     change(s) this build doesn't understand (relocated/dropped tables →");
        println!("     silent runtime errors):");
        for line in breaking_bullets(assessment) {
            println!("{line}");
        }
        println!("     Fix: switch to a version that knows these migrations, or close older");
        println!("     sessions before upgrading so they don't run degraded.");
    } else {
        println!(
            "  {} forward-additive only — safe.",
            console::style("✓").green()
        );
    }
}

/// Print the non-fatal "this version runs degraded" warning to stderr. Caller is
/// responsible for only invoking this when `assessment.degraded()` is true.
pub fn print_degraded_warning(version: &str, assessment: &Assessment) {
    let label = if version.is_empty() {
        "this build"
    } else {
        version
    };
    eprintln!();
    eprintln!(
        "  {}  Codex {} will run DEGRADED against your existing Codex state.",
        console::style("⚠").yellow(),
        console::style(label).yellow()
    );
    eprintln!(
        "     The on-disk DB (migration {}) was migrated by a newer version with breaking",
        assessment.db_max_applied
    );
    eprintln!("     change(s) this build doesn't understand:");
    for line in breaking_bullets(assessment) {
        eprintln!("{line}");
    }
    eprintln!("     Run `ovm doctor codex` for detail.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    // Build a fake "binary"/"db" whose bytes contain the given descriptions.
    fn blob_with(descriptions: &[&str]) -> Vec<u8> {
        let mut bytes = b"\x00\x7fELF junk".to_vec();
        for d in descriptions {
            bytes.extend_from_slice(d.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    fn write(dir: &Path, name: &str, descriptions: &[&str]) -> PathBuf {
        let path = dir.join(name);
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(&blob_with(descriptions)).unwrap();
        path
    }

    fn assess_blobs(applied: &[&str], known: &[&str]) -> Assessment {
        let dir = tempdir().unwrap();
        let state_db = write(dir.path(), "state_5.sqlite", applied);
        let binary = write(dir.path(), "codex", known);
        let applied = versions_present(std::slice::from_ref(&state_db), CODEX_STATE_MIGRATIONS);
        let known = versions_present(std::slice::from_ref(&binary), CODEX_STATE_MIGRATIONS);
        build_assessment(state_db, &applied, &known, CODEX_STATE_MIGRATIONS)
    }

    #[test]
    fn in_sync_binary_is_not_degraded() {
        let all: Vec<&str> = CODEX_STATE_MIGRATIONS
            .iter()
            .map(|m| m.description)
            .collect();
        let a = assess_blobs(&all, &all);
        assert!(!a.degraded());
        assert!(a.ahead.is_empty());
        assert_eq!(a.db_max_applied, 39);
    }

    #[test]
    fn old_binary_missing_breaking_migration_is_degraded() {
        let applied: Vec<&str> = CODEX_STATE_MIGRATIONS
            .iter()
            .map(|m| m.description)
            .collect();
        let known: Vec<&str> = CODEX_STATE_MIGRATIONS
            .iter()
            .filter(|m| m.version <= 34)
            .map(|m| m.description)
            .collect();
        let a = assess_blobs(&applied, &known);
        assert!(a.degraded(), "missing migration 35 should degrade");
        let breaking: Vec<u32> = a.breaking().map(|m| m.version).collect();
        assert_eq!(breaking, vec![35]);
        assert_eq!(a.binary_max_known, 34);
        assert_eq!(a.db_max_applied, 39);
    }

    #[test]
    fn additive_only_skew_is_not_degraded() {
        let applied: Vec<&str> = CODEX_STATE_MIGRATIONS
            .iter()
            .filter(|m| m.version <= 32)
            .map(|m| m.description)
            .collect();
        let known: Vec<&str> = CODEX_STATE_MIGRATIONS
            .iter()
            .filter(|m| m.version < 32)
            .map(|m| m.description)
            .collect();
        let a = assess_blobs(&applied, &known);
        assert!(!a.degraded());
        assert_eq!(a.additive().count(), 1);
    }

    // A binary that knows through the last breaking migration (35 "drop memory
    // tables") stays safe against today's v39 DB: 36–39 are all additive, so
    // e.g. codex v0.135–v0.139 run forward-compatible, not degraded.
    #[test]
    fn binary_at_last_breaking_is_safe_against_current_db() {
        let applied: Vec<&str> = CODEX_STATE_MIGRATIONS
            .iter()
            .map(|m| m.description)
            .collect();
        let known: Vec<&str> = CODEX_STATE_MIGRATIONS
            .iter()
            .filter(|m| m.version <= 35)
            .map(|m| m.description)
            .collect();
        let a = assess_blobs(&applied, &known);
        assert!(
            !a.degraded(),
            "36–39 are additive; binary@35 must stay safe"
        );
        assert_eq!(a.binary_max_known, 35);
        assert_eq!(a.db_max_applied, 39);
        assert_eq!(a.additive().count(), 4); // 36, 37, 38, 39
        assert_eq!(a.breaking().count(), 0);
    }
}
