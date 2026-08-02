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
//! the binary and stores migration records in the DB's `_sqlx_migrations`
//! table. We query the DB read-only and byte-scan only the binary — "migrations
//! this binary knows" vs authoritative "migrations the DB has applied" — then
//! flag applied migrations the binary doesn't understand, marking them breaking
//! via a manifest generated from Codex's open-source migration SQL.
//!
//! This crate is the single home for Codex schema-skew logic. When installed,
//! `ovm` invokes the `ovm-codex-skew` binary as Codex's optional companion at
//! lifecycle events (pre-launch, post-switch) and for `ovm doctor codex` — see
//! `crates/ovm/src/companions.rs`. Regenerate the manifest below with
//! `scripts/gen-codex-migration-manifest.py` when Codex adds migrations.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

#[derive(Debug)]
pub struct Migration {
    pub version: u32,
    pub description: &'static str,
    /// Removes a table/column an older binary still reads (a drop without a
    /// same-name recreate / rename). A rebuild that recreates the table under
    /// the same name is *not* breaking.
    pub breaking: bool,
}

// CODEX_STATE_MIGRATIONS_BEGIN
// Codex `state` migrator — generated from openai/codex codex-rs/state/migrations
// at rust-v0.146.0 (regenerate with scripts/gen-codex-migration-manifest.py).
// source commit: e363b08c9175ac1cbe5893615dd2cb9ddf95043b
// Keep in version
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
    Migration { version: 40, description: "threads history mode", breaking: false },
    Migration { version: 41, description: "threads name", breaking: false },
    Migration { version: 42, description: "drop agent jobs", breaking: true }, // removes agent_job_items, agent_jobs
    Migration { version: 43, description: "threads is pinned", breaking: false },
    Migration { version: 44, description: "external agent config imports provider id", breaking: false },
];
// CODEX_STATE_MIGRATIONS_END

/// The verdict of comparing one binary against the on-disk state DB.
#[derive(Debug)]
pub struct Assessment {
    pub state_db: PathBuf,
    pub db_max_applied: u32,
    pub binary_max_known: u32,
    /// Applied in the DB but unknown to this binary, in version order.
    pub ahead: Vec<&'static Migration>,
}

/// A check that could not establish a trustworthy compatibility verdict.
#[derive(Debug)]
pub struct Indeterminate {
    pub state_db: Option<PathBuf>,
    pub reason: String,
}

/// Typed result of checking a binary against Codex's shared state.
#[derive(Debug)]
pub enum AssessmentOutcome {
    NoStateDb,
    NoAppliedMigrations { state_db: PathBuf },
    Assessed(Assessment),
    Indeterminate(Indeterminate),
}

impl AssessmentOutcome {
    /// Stable machine-readable classification for qualification tooling.
    pub fn classification(&self) -> &'static str {
        match self {
            Self::NoStateDb | Self::NoAppliedMigrations { .. } => "compatible",
            Self::Assessed(assessment) if assessment.degraded() => "degraded",
            Self::Assessed(_) => "compatible",
            Self::Indeterminate(_) => "indeterminate",
        }
    }
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
    if let Some(path) = std::env::var_os("CODEX_HOME").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    dirs::home_dir().map(|home| home.join(".codex"))
}

/// Newest `state_<n>.sqlite` in `dir` (highest numeric suffix is the live one).
fn newest_state_db(dir: &Path) -> std::result::Result<Option<PathBuf>, Indeterminate> {
    let mut best: Option<(u32, PathBuf)> = None;
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Indeterminate {
                state_db: None,
                reason: format!("cannot read Codex state directory: {error}"),
            })
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| Indeterminate {
            state_db: None,
            reason: format!("cannot inspect Codex state directory: {error}"),
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
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
    Ok(best.map(|(_, path)| path))
}

/// Versions whose embedded description bytes appear in any candidate binary.
/// sqlx compiles migration descriptions verbatim into Codex, which is the only
/// evidence available for what an arbitrary installed binary knows.
///
/// Single O(n) pass per file: descriptions are bucketed by first byte, so most
/// positions skip with one comparison. A naive `windows()` scan over a ~25 MB
/// binary took ~minutes; this runs in milliseconds.
fn versions_present(paths: &[PathBuf], manifest: &[Migration]) -> std::io::Result<BTreeSet<u32>> {
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
        let bytes = fs::read(path)?;
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
    Ok(found)
}

/// Applied migrations come from SQLite's own table, opened read-only. This
/// sees committed WAL content correctly and never guesses from arbitrary bytes
/// that happen to resemble a migration description.
fn applied_migrations(state_db: &Path) -> std::result::Result<BTreeSet<u32>, Indeterminate> {
    let connection = Connection::open_with_flags(
        state_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| Indeterminate {
        state_db: Some(state_db.to_path_buf()),
        reason: format!("cannot open Codex state DB read-only: {error}"),
    })?;
    let mut statement = connection
        .prepare(
            "SELECT version, description
             FROM _sqlx_migrations
             WHERE success = TRUE
             ORDER BY version",
        )
        .map_err(|error| Indeterminate {
            state_db: Some(state_db.to_path_buf()),
            reason: format!("cannot read Codex migration table: {error}"),
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| Indeterminate {
            state_db: Some(state_db.to_path_buf()),
            reason: format!("cannot query Codex migration table: {error}"),
        })?;

    let mut versions = BTreeSet::new();
    for row in rows {
        let (version, description) = row.map_err(|error| Indeterminate {
            state_db: Some(state_db.to_path_buf()),
            reason: format!("malformed Codex migration row: {error}"),
        })?;
        let version = u32::try_from(version).map_err(|_| Indeterminate {
            state_db: Some(state_db.to_path_buf()),
            reason: format!("invalid Codex migration version: {version}"),
        })?;
        if let Some(expected) = CODEX_STATE_MIGRATIONS
            .iter()
            .find(|migration| migration.version == version)
        {
            if description != expected.description {
                return Err(Indeterminate {
                    state_db: Some(state_db.to_path_buf()),
                    reason: format!(
                        "migration {version} has unexpected description {description:?}"
                    ),
                });
            }
        }
        versions.insert(version);
    }
    Ok(versions)
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

fn assess_in_dir(binary: &Path, dir: &Path) -> AssessmentOutcome {
    let state_db = match newest_state_db(dir) {
        Ok(Some(path)) => path,
        Ok(None) => return AssessmentOutcome::NoStateDb,
        Err(error) => return AssessmentOutcome::Indeterminate(error),
    };
    let applied = match applied_migrations(&state_db) {
        Ok(applied) => applied,
        Err(error) => return AssessmentOutcome::Indeterminate(error),
    };
    if applied.is_empty() {
        return AssessmentOutcome::NoAppliedMigrations { state_db };
    }
    if let Some(version) = applied.iter().find(|version| {
        !CODEX_STATE_MIGRATIONS
            .iter()
            .any(|item| item.version == **version)
    }) {
        let db_max = max_version(&applied);
        let guard_max = CODEX_STATE_MIGRATIONS
            .last()
            .map(|migration| migration.version)
            .unwrap_or(0);
        return AssessmentOutcome::Indeterminate(Indeterminate {
            state_db: Some(state_db),
            reason: format!(
                "state DB is at migration {db_max}, guard knows through {guard_max}; first unknown migration is {version}"
            ),
        });
    }
    let known = match versions_present(&[binary.to_path_buf()], CODEX_STATE_MIGRATIONS) {
        Ok(known) => known,
        Err(error) => {
            return AssessmentOutcome::Indeterminate(Indeterminate {
                state_db: Some(state_db),
                reason: format!("cannot read Codex binary: {error}"),
            })
        }
    };

    AssessmentOutcome::Assessed(build_assessment(
        state_db,
        &applied,
        &known,
        CODEX_STATE_MIGRATIONS,
    ))
}

/// Compare a Codex `binary` against the live `~/.codex` state DB.
pub fn assess(binary: &Path) -> AssessmentOutcome {
    let Some(dir) = state_dir() else {
        return AssessmentOutcome::Indeterminate(Indeterminate {
            state_db: None,
            reason: "cannot determine the home directory for Codex state".to_string(),
        });
    };
    assess_in_dir(binary, &dir)
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

/// Detailed report for `ovm doctor codex`, including explicit indeterminate
/// outcomes, printed to stdout.
pub fn print_report(version: &str, binary: &Path, outcome: &AssessmentOutcome) {
    let label = if version.is_empty() {
        "active"
    } else {
        version
    };
    println!("schema-guard · Codex {label}");
    println!("  binary   : {}", binary.display());

    let assessment = match outcome {
        AssessmentOutcome::NoStateDb => {
            println!(
                "  {} no Codex state DB found yet — nothing to check.",
                console::style("✓").green()
            );
            return;
        }
        AssessmentOutcome::NoAppliedMigrations { state_db } => {
            println!("  state db : {}", state_db.display());
            println!(
                "  {} no applied Codex migrations yet — nothing to check.",
                console::style("✓").green()
            );
            return;
        }
        AssessmentOutcome::Indeterminate(indeterminate) => {
            if let Some(state_db) = &indeterminate.state_db {
                println!("  state db : {}", state_db.display());
            }
            println!(
                "  {} INDETERMINATE — {}",
                console::style("?").yellow(),
                indeterminate.reason
            );
            return;
        }
        AssessmentOutcome::Assessed(assessment) => assessment,
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

/// Print a non-fatal warning when the guard cannot establish a safe verdict.
pub fn print_indeterminate_warning(indeterminate: &Indeterminate) {
    eprintln!();
    eprintln!(
        "  {}  Codex schema check is INDETERMINATE: {}",
        console::style("?").yellow(),
        indeterminate.reason
    );
    eprintln!("     Launch continues because this companion is advisory.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
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
        let applied = versions_present(std::slice::from_ref(&state_db), CODEX_STATE_MIGRATIONS)
            .expect("read applied blob");
        let known = versions_present(std::slice::from_ref(&binary), CODEX_STATE_MIGRATIONS)
            .expect("read binary blob");
        build_assessment(state_db, &applied, &known, CODEX_STATE_MIGRATIONS)
    }

    fn state_db(dir: &Path, rows: &[(u32, &str, bool)]) -> PathBuf {
        let path = dir.join("state_5.sqlite");
        let connection = Connection::open(&path).expect("create sqlite state");
        connection
            .execute_batch(
                "CREATE TABLE _sqlx_migrations (
                    version BIGINT PRIMARY KEY,
                    description TEXT NOT NULL,
                    installed_on TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    success BOOLEAN NOT NULL,
                    checksum BLOB NOT NULL,
                    execution_time BIGINT NOT NULL
                );",
            )
            .expect("create migrations table");
        for (version, description, success) in rows {
            connection
                .execute(
                    "INSERT INTO _sqlx_migrations
                     (version, description, success, checksum, execution_time)
                     VALUES (?1, ?2, ?3, X'00', 1)",
                    rusqlite::params![version, description, success],
                )
                .expect("insert migration");
        }
        drop(connection);
        path
    }

    #[test]
    fn sqlite_migration_table_is_authoritative_and_read_only() {
        let dir = tempdir().expect("tempdir");
        let db = state_db(
            dir.path(),
            &[
                (34, "drop thread goals", true),
                (35, "drop memory tables", true),
                (36, "threads visible sort indexes", false),
            ],
        );
        let binary = write(dir.path(), "codex", &["drop thread goals"]);

        let outcome = assess_in_dir(&binary, dir.path());
        let AssessmentOutcome::Assessed(assessment) = outcome else {
            panic!("expected assessed outcome");
        };
        assert_eq!(assessment.db_max_applied, 35);
        assert_eq!(
            assessment.breaking().map(|m| m.version).collect::<Vec<_>>(),
            vec![35]
        );
        assert!(!db.with_extension("sqlite-wal").exists());
    }

    #[test]
    fn malformed_sqlite_state_is_indeterminate() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(dir.path().join("state_5.sqlite"), b"not sqlite").expect("malformed db");
        let binary = write(dir.path(), "codex", &["threads"]);

        assert!(matches!(
            assess_in_dir(&binary, dir.path()),
            AssessmentOutcome::Indeterminate(_)
        ));
    }

    #[test]
    fn malformed_migration_record_is_indeterminate() {
        let dir = tempdir().expect("tempdir");
        state_db(dir.path(), &[(35, "not the real migration", true)]);
        let binary = write(dir.path(), "codex", &["drop memory tables"]);

        assert!(matches!(
            assess_in_dir(&binary, dir.path()),
            AssessmentOutcome::Indeterminate(_)
        ));
    }

    #[test]
    fn unknown_migration_reports_database_and_guard_bounds() {
        let dir = tempdir().expect("tempdir");
        let future = guard_max() + 1;
        state_db(dir.path(), &[(future, "future migration", true)]);
        let binary = write(dir.path(), "codex", &["future migration"]);

        let AssessmentOutcome::Indeterminate(indeterminate) = assess_in_dir(&binary, dir.path())
        else {
            panic!("expected indeterminate outcome");
        };
        // This exact sentence is what tells an operator the guard is stale
        // rather than the release being suspect. Codex rust-v0.146.0 was
        // withheld for two months' worth of migrations past this ceiling, and
        // the reason never reached anyone because the caller printed only the
        // word "indeterminate".
        assert_eq!(
            indeterminate.reason,
            format!(
                "state DB is at migration {future}, guard knows through {}; \
                 first unknown migration is {future}",
                guard_max()
            )
        );
    }

    #[test]
    fn unreadable_or_missing_binary_is_indeterminate() {
        let dir = tempdir().expect("tempdir");
        state_db(dir.path(), &[(1, "threads", true)]);

        assert!(matches!(
            assess_in_dir(&dir.path().join("missing-codex"), dir.path()),
            AssessmentOutcome::Indeterminate(_)
        ));
    }

    /// The newest migration the guard knows. Derived, never hardcoded: a
    /// manifest sync moves this, and a test that pins the old number fails for
    /// the wrong reason and teaches you to edit the number instead of asking
    /// why the ceiling moved.
    fn guard_max() -> u32 {
        CODEX_STATE_MIGRATIONS
            .last()
            .expect("manifest is non-empty")
            .version
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
        assert_eq!(a.db_max_applied, guard_max());
    }

    #[test]
    fn recent_migrations_are_backfilled() {
        let tail = CODEX_STATE_MIGRATIONS
            .iter()
            .filter(|migration| migration.version >= 40)
            .map(|migration| (migration.version, migration.description, migration.breaking))
            .collect::<Vec<_>>();

        assert_eq!(
            tail,
            vec![
                (40, "threads history mode", false),
                (41, "threads name", false),
                (42, "drop agent jobs", true),
                (43, "threads is pinned", false),
                (44, "external agent config imports provider id", false),
            ]
        );
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
        assert_eq!(breaking, vec![35, 42]);
        assert_eq!(a.binary_max_known, 34);
        assert_eq!(a.db_max_applied, guard_max());
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

    // Migration 42 became the next destructive boundary after migration 35.
    #[test]
    fn binary_at_previous_breaking_is_degraded_by_migration_42() {
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
        assert!(a.degraded(), "migration 42 drops the agent-job tables");
        assert_eq!(a.binary_max_known, 35);
        assert_eq!(a.db_max_applied, guard_max());
        assert_eq!(a.additive().count(), 8); // 36–41, 43, 44
        assert_eq!(
            a.breaking().map(|m| m.version).collect::<Vec<_>>(),
            vec![42]
        );
    }

    #[test]
    fn assessment_outcomes_have_stable_machine_classifications() {
        assert_eq!(AssessmentOutcome::NoStateDb.classification(), "compatible");
        assert_eq!(
            AssessmentOutcome::Indeterminate(Indeterminate {
                state_db: None,
                reason: "fixture".into(),
            })
            .classification(),
            "indeterminate"
        );
    }
}
