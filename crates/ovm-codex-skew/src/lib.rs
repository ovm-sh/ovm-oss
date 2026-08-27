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
//! Two things outrank that static inference, both read from one served
//! document (`codex-skew.json`, published by the observatory next to the
//! version registry and cached by `ovm`'s background refresh):
//!
//! * a **served manifest** — the same generated table, republished within a
//!   cycle of each Codex stable, so a released OVM keeps classifying migrations
//!   that did not exist when it was built instead of reporting INDETERMINATE
//!   until the next OVM release; and
//! * **observed verdicts** — the ladder runs each older Codex stable against a
//!   state DB migrated by the newest one and records whether it actually
//!   degraded. An observation outranks the static guess in both directions: a
//!   DROP the ladder passed stays quiet, and a regression the ladder saw warns
//!   even when no migration looks breaking.
//!
//! Without the document the guard behaves exactly as before, from the compiled
//! manifest alone. It never fetches anything itself.
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
// at rust-v0.150.1 (regenerate with scripts/gen-codex-migration-manifest.py).
// source commit: 90854393966b21e9ebfd21b122334eb09a20c93d
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
    Migration { version: 45, description: "threads section", breaking: false },
    Migration { version: 46, description: "threads section order", breaking: false },
    Migration { version: 47, description: "rollout migration state", breaking: false },
    Migration { version: 48, description: "thread section appearance", breaking: false },
    Migration { version: 49, description: "projects", breaking: false },
    Migration { version: 50, description: "threads section empty preview indexes", breaking: false },
    Migration { version: 51, description: "thread artifacts", breaking: false },
];
// CODEX_STATE_MIGRATIONS_END

/// A migration the guard knows about at runtime — the compiled manifest plus
/// whatever newer entries the served document adds. Owned, because served
/// descriptions arrive at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownMigration {
    pub version: u32,
    pub description: String,
    pub breaking: bool,
}

impl From<&Migration> for KnownMigration {
    fn from(migration: &Migration) -> Self {
        Self {
            version: migration.version,
            description: migration.description.to_string(),
            breaking: migration.breaking,
        }
    }
}

/// The manifest compiled into this build.
pub fn compiled_manifest() -> Vec<KnownMigration> {
    CODEX_STATE_MIGRATIONS
        .iter()
        .map(KnownMigration::from)
        .collect()
}

/// Newest migration version the compiled manifest knows.
pub fn compiled_max() -> u32 {
    CODEX_STATE_MIGRATIONS
        .last()
        .map(|migration| migration.version)
        .unwrap_or(0)
}

/// What the observatory's ladder saw when it ran a Codex version against a
/// state DB migrated up to `db_migration`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub db_migration: u32,
    pub version: String,
    pub verdict: ObservedVerdict,
    pub runs: u64,
    pub run_number: Option<u64>,
    pub observed_at: Option<String>,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedVerdict {
    Compatible,
    Degraded,
    Broken,
}

impl ObservedVerdict {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "compatible" => Some(Self::Compatible),
            "degraded" => Some(Self::Degraded),
            "broken" => Some(Self::Broken),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compatible => "compatible",
            Self::Degraded => "degraded",
            Self::Broken => "broken",
        }
    }
}

/// The served `codex-skew.json` document, already validated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evidence {
    pub generated_at: Option<String>,
    pub manifest_source_ref: Option<String>,
    /// The served manifest, in version order.
    pub migrations: Vec<KnownMigration>,
    pub observed: Vec<Observation>,
}

const EVIDENCE_SCHEMA_VERSION: u64 = 1;
pub const EVIDENCE_FILE_NAME: &str = "codex-skew.json";

/// Bounds on what a served document may make the guard do at launch. The
/// byte cap bounds I/O; these bound work and output, so a malformed or hostile
/// document that still parses cannot stall a launch (the binary scan is
/// proportional to the manifest it searches for) or flood stderr.
///
/// Codex adds a couple of migrations a week; 128 served entries past the
/// compiled ceiling is a year of headroom, and an OVM release lands well
/// inside that.
pub const EVIDENCE_MAX_SERVED_MIGRATIONS: usize = 128;
pub const EVIDENCE_MAX_DESCRIPTION_BYTES: usize = 128;
pub const EVIDENCE_MAX_OBSERVATIONS: usize = 1024;
pub const EVIDENCE_MAX_EVIDENCE_LINES: usize = 8;
pub const EVIDENCE_MAX_LINE_CHARS: usize = 300;

/// Env var `ovm` sets for companions: the directory holding cached registry
/// documents (`~/.ovm/cache/registry`).
pub const REGISTRY_CACHE_ENV: &str = "OVM_REGISTRY_CACHE";

/// Where the cached evidence document lives: the directory `ovm` names in
/// [`REGISTRY_CACHE_ENV`], else the default OVM cache.
pub fn default_evidence_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(REGISTRY_CACHE_ENV).filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir).join(EVIDENCE_FILE_NAME));
    }
    dirs::home_dir().map(|home| {
        home.join(".ovm")
            .join("cache")
            .join("registry")
            .join(EVIDENCE_FILE_NAME)
    })
}

/// Largest evidence document the guard will read. The real one is ~20 KB; a
/// cache file past this is not evidence, and the launch path must never block
/// on reading it (the whole document is parsed into a `serde_json::Value`, so
/// the byte cap also bounds transient memory).
pub const EVIDENCE_MAX_BYTES: u64 = 1024 * 1024;

/// Read and validate the served document. `None` whenever it is absent or
/// unusable — the guard then runs from the compiled manifest alone.
///
/// The file is opened once and inspected through that descriptor (no
/// check-then-reopen window), opened non-blocking on Unix so a FIFO at the
/// path cannot hang the launch, refused unless it is a regular file, and read
/// bounded at [`EVIDENCE_MAX_BYTES`] — a file that grows after the size check
/// is cut off, not swallowed.
pub fn load_evidence(path: &Path) -> Option<Evidence> {
    use std::io::Read;

    let file = open_evidence_file(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > EVIDENCE_MAX_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(EVIDENCE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > EVIDENCE_MAX_BYTES {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    parse_evidence(&text)
}

#[cfg(unix)]
fn open_evidence_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    // O_NONBLOCK: opening a FIFO for reading would otherwise block until a
    // writer appears. Regular files ignore the flag.
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_evidence_file(path: &Path) -> std::io::Result<fs::File> {
    fs::File::open(path)
}

/// Parse the served document leniently: the document is refused only when it
/// is not a schema-1 `codex` object; a malformed entry or a mistyped optional
/// field drops that entry (or field), never the whole document. `ovm` caches a
/// document only after a stricter structural check, so anything it caches is
/// accepted here — a cached document can never be one the guard rejects.
pub fn parse_evidence(text: &str) -> Option<Evidence> {
    let document: serde_json::Value = serde_json::from_str(text).ok()?;
    let object = document.as_object()?;
    if object.get("schema_version").and_then(|v| v.as_u64()) != Some(EVIDENCE_SCHEMA_VERSION)
        || object.get("product").and_then(|v| v.as_str()) != Some("codex")
    {
        return None;
    }
    let clip = |text: &str| -> String { text.chars().take(EVIDENCE_MAX_LINE_CHARS).collect() };
    let string = |value: Option<&serde_json::Value>| value.and_then(|v| v.as_str()).map(clip);
    let manifest = object.get("manifest").and_then(|v| v.as_object());

    // Only entries that can extend the compiled manifest are worth keeping,
    // and only a bounded number of them: the binary scan's cost is
    // proportional to the manifest it searches for.
    let ceiling = compiled_max() as usize + EVIDENCE_MAX_SERVED_MIGRATIONS;
    let mut migrations: Vec<KnownMigration> = manifest
        .and_then(|m| m.get("migrations"))
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let entry = entry.as_object()?;
            let version = u32::try_from(entry.get("version")?.as_u64()?).ok()?;
            if version as usize > ceiling {
                return None;
            }
            let description = entry.get("description")?.as_str()?;
            if description.is_empty() || description.len() > EVIDENCE_MAX_DESCRIPTION_BYTES {
                return None;
            }
            Some(KnownMigration {
                version,
                description: description.to_string(),
                breaking: entry.get("breaking")?.as_bool()?,
            })
        })
        .collect();
    migrations.sort_by_key(|migration| migration.version);
    migrations.dedup_by_key(|migration| migration.version);

    let observed = object
        .get("observed")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let entry = entry.as_object()?;
            Some(Observation {
                db_migration: u32::try_from(entry.get("db_migration")?.as_u64()?).ok()?,
                version: clip(entry.get("version")?.as_str()?),
                // An indeterminate ladder verdict is not evidence either way.
                verdict: ObservedVerdict::parse(entry.get("verdict")?.as_str()?)?,
                runs: entry.get("runs").and_then(|v| v.as_u64()).unwrap_or(0),
                run_number: entry.get("run_number").and_then(|v| v.as_u64()),
                observed_at: string(entry.get("observed_at")),
                evidence: entry
                    .get("evidence")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|line| line.as_str().map(clip))
                    .take(EVIDENCE_MAX_EVIDENCE_LINES)
                    .collect(),
            })
        })
        .take(EVIDENCE_MAX_OBSERVATIONS)
        .collect();

    Some(Evidence {
        generated_at: string(object.get("generated_at")),
        manifest_source_ref: string(manifest.and_then(|m| m.get("source_ref"))),
        migrations,
        observed,
    })
}

/// The manifest the guard reasons with: compiled entries verbatim, then served
/// entries that continue the sequence past the compiled ceiling. Served entries
/// never rewrite compiled history, and a gap in the served tail ends the
/// extension (the contiguity invariant is what makes "guard knows through N"
/// meaningful).
pub fn effective_manifest(evidence: Option<&Evidence>) -> Vec<KnownMigration> {
    let mut manifest = compiled_manifest();
    let Some(evidence) = evidence else {
        return manifest;
    };
    let mut next = compiled_max() + 1;
    for migration in &evidence.migrations {
        if migration.version < next {
            continue;
        }
        if migration.version != next {
            break;
        }
        manifest.push(migration.clone());
        next += 1;
    }
    manifest
}

/// The verdict of comparing one binary against the on-disk state DB.
#[derive(Debug, PartialEq, Eq)]
pub struct Assessment {
    pub state_db: PathBuf,
    pub db_max_applied: u32,
    pub binary_max_known: u32,
    /// Applied in the DB but unknown to this binary, in version order.
    pub ahead: Vec<KnownMigration>,
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

    pub fn breaking(&self) -> impl Iterator<Item = &KnownMigration> + '_ {
        self.ahead.iter().filter(|m| m.breaking)
    }

    pub fn additive(&self) -> impl Iterator<Item = &KnownMigration> + '_ {
        self.ahead.iter().filter(|m| !m.breaking)
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

/// How long the binary scan may run before the guard gives up and reports
/// INDETERMINATE instead of holding a launch. A real scan of a ~25 MiB Codex
/// binary takes milliseconds; only a pathological served manifest (many long
/// descriptions sharing a prefix that recurs in the binary) approaches this,
/// and such a manifest must not be able to stall every launch.
const BINARY_SCAN_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
/// The deadline is checked every this many candidate comparisons — work done,
/// not input consumed — so a region of the binary that is expensive to scan
/// cannot overshoot the budget between checks, and a small binary still gets
/// checked.
const BINARY_SCAN_CHECK_EVERY_ATTEMPTS: u64 = 1 << 16;

/// Versions whose embedded description bytes appear in any candidate binary.
/// sqlx compiles migration descriptions verbatim into Codex, which is the only
/// evidence available for what an arbitrary installed binary knows.
///
/// Single O(n) pass per file: descriptions are bucketed by first byte, so most
/// positions skip with one comparison. A naive `windows()` scan over a ~25 MB
/// binary took ~minutes; this runs in milliseconds.
fn versions_present(
    paths: &[PathBuf],
    manifest: &[KnownMigration],
) -> std::io::Result<BTreeSet<u32>> {
    versions_present_until(
        paths,
        manifest,
        std::time::Instant::now() + BINARY_SCAN_BUDGET,
    )
}

/// [`versions_present`] with an explicit deadline; past it the scan stops with
/// `ErrorKind::TimedOut`. Deadline-checked once per
/// [`BINARY_SCAN_CHECK_EVERY_ATTEMPTS`] candidate comparisons, so the check
/// itself is free and the overshoot past the deadline is bounded by that many
/// comparisons.
fn versions_present_until(
    paths: &[PathBuf],
    manifest: &[KnownMigration],
    deadline: std::time::Instant,
) -> std::io::Result<BTreeSet<u32>> {
    let mut buckets: [Vec<&KnownMigration>; 256] = std::array::from_fn(|_| Vec::new());
    let mut remaining = 0;
    for migration in manifest {
        if let Some(&first) = migration.description.as_bytes().first() {
            buckets[first as usize].push(migration);
            remaining += 1;
        }
    }

    let mut found = BTreeSet::new();
    let mut attempts: u64 = 0;
    'scan: for path in paths {
        let bytes = fs::read(path)?;
        for (i, &byte) in bytes.iter().enumerate() {
            let candidates = &mut buckets[byte as usize];
            let mut j = 0;
            while j < candidates.len() {
                attempts += 1;
                if attempts.is_multiple_of(BINARY_SCAN_CHECK_EVERY_ATTEMPTS)
                    && std::time::Instant::now() >= deadline
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "binary scan exceeded its time budget",
                    ));
                }
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
fn applied_migrations(
    state_db: &Path,
    manifest: &[KnownMigration],
) -> std::result::Result<BTreeSet<u32>, Indeterminate> {
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
        if let Some(expected) = manifest
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
    manifest: &[KnownMigration],
) -> Assessment {
    Assessment {
        state_db,
        db_max_applied: max_version(applied),
        binary_max_known: max_version(known),
        ahead: manifest
            .iter()
            .filter(|m| applied.contains(&m.version) && !known.contains(&m.version))
            .cloned()
            .collect(),
    }
}

fn assess_in_dir(binary: &Path, dir: &Path, manifest: &[KnownMigration]) -> AssessmentOutcome {
    let state_db = match newest_state_db(dir) {
        Ok(Some(path)) => path,
        Ok(None) => return AssessmentOutcome::NoStateDb,
        Err(error) => return AssessmentOutcome::Indeterminate(error),
    };
    let applied = match applied_migrations(&state_db, manifest) {
        Ok(applied) => applied,
        Err(error) => return AssessmentOutcome::Indeterminate(error),
    };
    if applied.is_empty() {
        return AssessmentOutcome::NoAppliedMigrations { state_db };
    }
    if let Some(version) = applied
        .iter()
        .find(|version| !manifest.iter().any(|item| item.version == **version))
    {
        let db_max = max_version(&applied);
        let guard_max = manifest
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
    let known = match versions_present(&[binary.to_path_buf()], manifest) {
        Ok(known) => known,
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            return AssessmentOutcome::Indeterminate(Indeterminate {
                state_db: Some(state_db),
                reason: format!("Codex binary scan gave up: {error}"),
            })
        }
        Err(error) => {
            return AssessmentOutcome::Indeterminate(Indeterminate {
                state_db: Some(state_db),
                reason: format!("cannot read Codex binary: {error}"),
            })
        }
    };

    AssessmentOutcome::Assessed(build_assessment(state_db, &applied, &known, manifest))
}

/// Compare a Codex `binary` against the live `~/.codex` state DB using the
/// compiled manifest alone. This is the static classifier the observatory's
/// ladder records as `staticCompatibility`; it deliberately ignores served
/// evidence so "static" keeps meaning "what the shipped guard infers".
pub fn assess(binary: &Path) -> AssessmentOutcome {
    assess_with_manifest(binary, &compiled_manifest())
}

fn assess_with_manifest(binary: &Path, manifest: &[KnownMigration]) -> AssessmentOutcome {
    let Some(dir) = state_dir() else {
        return AssessmentOutcome::Indeterminate(Indeterminate {
            state_db: None,
            reason: "cannot determine the home directory for Codex state".to_string(),
        });
    };
    assess_in_dir(binary, &dir, manifest)
}

/// Where the manifest the guard reasoned with came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestSource {
    Compiled,
    /// The served document extended the compiled manifest to `through`.
    Served {
        source_ref: Option<String>,
        through: u32,
    },
}

/// The full guard result: the static assessment plus, when the observatory
/// has run this version against a comparable DB, what it saw.
#[derive(Debug)]
pub struct Guard {
    pub outcome: AssessmentOutcome,
    pub observed: Option<Observation>,
    pub manifest_source: ManifestSource,
    pub evidence_generated_at: Option<String>,
}

/// What the launch path should say, if anything.
#[derive(Debug, PartialEq, Eq)]
pub enum LaunchVerdict<'a> {
    /// Nothing worth interrupting a launch for: in sync, additive-only,
    /// observed compatible, or not knowable right now (an indeterminate check
    /// is something `ovm doctor codex` explains; it is not a launch warning).
    Silent,
    /// The observatory ran this version against a comparable DB and saw it
    /// degrade.
    ObservedDegraded(&'a Observation),
    /// No observation; the compiled/served manifest says a breaking migration
    /// is ahead of this binary.
    StaticDegraded(&'a Assessment),
}

/// Run the guard for `version`'s `binary` with optional served evidence.
pub fn guard(binary: &Path, version: &str, evidence: Option<&Evidence>) -> Guard {
    match state_dir() {
        Some(dir) => guard_in_dir(binary, &dir, version, evidence),
        None => Guard {
            outcome: AssessmentOutcome::Indeterminate(Indeterminate {
                state_db: None,
                reason: "cannot determine the home directory for Codex state".to_string(),
            }),
            observed: None,
            manifest_source: ManifestSource::Compiled,
            evidence_generated_at: evidence.and_then(|e| e.generated_at.clone()),
        },
    }
}

fn guard_in_dir(binary: &Path, dir: &Path, version: &str, evidence: Option<&Evidence>) -> Guard {
    let manifest = effective_manifest(evidence);
    let manifest_source = match manifest.last() {
        Some(last) if last.version > compiled_max() => ManifestSource::Served {
            source_ref: evidence.and_then(|e| e.manifest_source_ref.clone()),
            through: last.version,
        },
        _ => ManifestSource::Compiled,
    };
    let outcome = assess_in_dir(binary, dir, &manifest);
    let observed = match (&outcome, evidence) {
        (AssessmentOutcome::Assessed(assessment), Some(evidence)) => {
            find_observation(evidence, &manifest, assessment.db_max_applied, version).cloned()
        }
        _ => None,
    };
    Guard {
        outcome,
        observed,
        manifest_source,
        evidence_generated_at: evidence.and_then(|e| e.generated_at.clone()),
    }
}

impl Guard {
    pub fn launch_verdict(&self) -> LaunchVerdict<'_> {
        if let Some(observation) = &self.observed {
            return match observation.verdict {
                ObservedVerdict::Compatible => LaunchVerdict::Silent,
                ObservedVerdict::Degraded | ObservedVerdict::Broken => {
                    LaunchVerdict::ObservedDegraded(observation)
                }
            };
        }
        match &self.outcome {
            AssessmentOutcome::Assessed(assessment) if assessment.degraded() => {
                LaunchVerdict::StaticDegraded(assessment)
            }
            _ => LaunchVerdict::Silent,
        }
    }
}

/// The observation that applies to running `version` against a DB at
/// `db_max`.
///
/// An observation at that EXACT migration is the only direct evidence and
/// always wins (newest run among several). An "additive" migration only
/// promises no schema removal, not that behavior is monotone across it — a
/// later migration could recreate or repair what an older binary was missing —
/// so a pass observed at a higher migration proves nothing about this one.
///
/// Without an exact observation the guard therefore reaches forward only for
/// the pessimistic half: a DEGRADED/BROKEN observation at the nearest later
/// migration the DB could reach by additive migrations alone is worth warning
/// about (the ladder always observes at the newest migration; users lag behind
/// it). A compatible observation up there is never used to silence the static
/// verdict for this DB.
fn find_observation<'a>(
    evidence: &'a Evidence,
    manifest: &[KnownMigration],
    db_max: u32,
    version: &str,
) -> Option<&'a Observation> {
    let for_version = || evidence.observed.iter().filter(|o| o.version == version);
    let newest = |a: &&Observation, b: &&Observation| {
        (
            a.run_number.unwrap_or(0),
            a.observed_at.as_deref().unwrap_or(""),
        )
            .cmp(&(
                b.run_number.unwrap_or(0),
                b.observed_at.as_deref().unwrap_or(""),
            ))
    };
    if let Some(exact) = for_version()
        .filter(|o| o.db_migration == db_max)
        .max_by(newest)
    {
        return Some(exact);
    }
    // The furthest migration reachable from db_max by additive steps alone,
    // computed once: a walk up the (version-ordered) manifest, not a rescan
    // per observation.
    let reach_end = {
        let start = db_max + 1;
        let mut next = start;
        for m in manifest.iter().skip_while(|m| m.version < start) {
            if m.version != next || m.breaking {
                break;
            }
            next += 1;
        }
        next - 1
    };
    let additive_gap = |at: u32| at > db_max && at <= reach_end;
    let nearest = for_version()
        .filter(|o| additive_gap(o.db_migration))
        .map(|o| o.db_migration)
        .min()?;
    for_version()
        .filter(|o| o.db_migration == nearest)
        .max_by(newest)
        .filter(|o| o.verdict != ObservedVerdict::Compatible)
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

fn observation_summary(observation: &Observation) -> String {
    let runs = match observation.runs {
        1 => "1 run".to_string(),
        n => format!("{n} runs"),
    };
    let when = observation
        .observed_at
        .as_deref()
        .map(|stamp| stamp.get(..10).unwrap_or(stamp).to_string())
        .map(|day| format!(", last {day}"))
        .unwrap_or_default();
    format!(
        "{} against a DB at migration {} ({runs}{when})",
        observation.verdict.as_str(),
        observation.db_migration
    )
}

/// Detailed report for `ovm doctor codex`, including explicit indeterminate
/// outcomes, printed to stdout.
pub fn print_report(version: &str, binary: &Path, guard: &Guard) {
    let label = if version.is_empty() {
        "active"
    } else {
        version
    };
    println!("schema-guard · Codex {label}");
    println!("  binary   : {}", binary.display());
    match &guard.manifest_source {
        ManifestSource::Compiled => {
            println!(
                "  manifest : compiled, knows migrations through {}",
                compiled_max()
            );
        }
        ManifestSource::Served {
            source_ref,
            through,
        } => {
            let from = source_ref
                .as_deref()
                .map(|r| format!(" from {r}"))
                .unwrap_or_default();
            println!(
                "  manifest : compiled through {}, served{from} extends it through {through}",
                compiled_max()
            );
        }
    }
    if let Some(stamp) = &guard.evidence_generated_at {
        println!(
            "  evidence : observatory ledger as of {}",
            stamp.get(..10).unwrap_or(stamp)
        );
    }

    let assessment = match &guard.outcome {
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

    if let Some(observation) = &guard.observed {
        println!("  observed : {}", observation_summary(observation));
    }

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

    match guard.launch_verdict() {
        LaunchVerdict::ObservedDegraded(observation) => {
            println!(
                "  {}  OBSERVED DEGRADED — the observatory ran this version against a DB at",
                console::style("⚠").yellow()
            );
            println!(
                "     migration {} and saw it degrade:",
                observation.db_migration
            );
            for line in &observation.evidence {
                println!("       {} {line}", console::style("·").yellow());
            }
            println!("     Fix: switch to a version that knows these migrations, or close older");
            println!("     sessions before upgrading so they don't run degraded.");
        }
        LaunchVerdict::StaticDegraded(_) => {
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
        }
        LaunchVerdict::Silent if assessment.degraded() => {
            println!(
                "  {} observed compatible — static analysis flags breaking migration(s), but",
                console::style("✓").green()
            );
            println!("     the observatory ran this version against a comparable DB and saw no");
            println!("     degradation; the observation wins:");
            for line in breaking_bullets(assessment) {
                println!("{line}");
            }
        }
        LaunchVerdict::Silent => {
            println!(
                "  {} forward-additive only — safe.",
                console::style("✓").green()
            );
        }
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

/// Print the non-fatal "the observatory saw this version degrade" warning to
/// stderr.
pub fn print_observed_warning(version: &str, observation: &Observation) {
    let label = if version.is_empty() {
        "this build"
    } else {
        version
    };
    eprintln!();
    eprintln!(
        "  {}  Codex {} was observed {} against your Codex state.",
        console::style("⚠").yellow(),
        console::style(label).yellow(),
        observation.verdict.as_str()
    );
    eprintln!(
        "     OVM's observatory ran it against a DB at migration {} and saw:",
        observation.db_migration
    );
    for line in &observation.evidence {
        eprintln!("       {} {line}", console::style("·").yellow());
    }
    eprintln!("     Run `ovm doctor codex` for detail.");
}

/// Print a non-fatal warning when the guard cannot establish a safe verdict.
/// Not used on the launch path (an indeterminate check is a doctor topic, not
/// a launch interruption); kept for callers that want the explicit line.
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
        let manifest = compiled_manifest();
        let applied = versions_present(std::slice::from_ref(&state_db), &manifest)
            .expect("read applied blob");
        let known =
            versions_present(std::slice::from_ref(&binary), &manifest).expect("read binary blob");
        build_assessment(state_db, &applied, &known, &manifest)
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

    fn assess_compiled(binary: &Path, dir: &Path) -> AssessmentOutcome {
        assess_in_dir(binary, dir, &compiled_manifest())
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

        let outcome = assess_compiled(&binary, dir.path());
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
            assess_compiled(&binary, dir.path()),
            AssessmentOutcome::Indeterminate(_)
        ));
    }

    #[test]
    fn malformed_migration_record_is_indeterminate() {
        let dir = tempdir().expect("tempdir");
        state_db(dir.path(), &[(35, "not the real migration", true)]);
        let binary = write(dir.path(), "codex", &["drop memory tables"]);

        assert!(matches!(
            assess_compiled(&binary, dir.path()),
            AssessmentOutcome::Indeterminate(_)
        ));
    }

    #[test]
    fn unknown_migration_reports_database_and_guard_bounds() {
        let dir = tempdir().expect("tempdir");
        let future = guard_max() + 1;
        state_db(dir.path(), &[(future, "future migration", true)]);
        let binary = write(dir.path(), "codex", &["future migration"]);

        let AssessmentOutcome::Indeterminate(indeterminate) = assess_compiled(&binary, dir.path())
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
            assess_compiled(&dir.path().join("missing-codex"), dir.path()),
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

    /// Reviewed history stays pinned exactly; the tail past it is open.
    ///
    /// Upstream ships new state migrations constantly, and re-pinning the
    /// full list made every additive release a manual chore. Policy since
    /// 2026-08-07: NON-BREAKING migrations flow through the sync + publish
    /// pipeline automatically; only a breaking or unclassifiable migration
    /// stops the line ([`old_binary_missing_breaking_migration_is_degraded`]
    /// pins the complete breaking set, and the workflow fails on the
    /// classifier's BREAKING/INDETERMINATE verdicts). What this test still
    /// owns: history that was reviewed by a human must never be rewritten,
    /// and every generated entry — reviewed or not — must be structurally
    /// sound (contiguous versions, non-empty descriptions).
    #[test]
    fn reviewed_history_is_pinned_and_the_generated_tail_is_sound() {
        const LAST_REVIEWED: u32 = 46;
        let reviewed = CODEX_STATE_MIGRATIONS
            .iter()
            .filter(|migration| (40..=LAST_REVIEWED).contains(&migration.version))
            .map(|migration| (migration.version, migration.description, migration.breaking))
            .collect::<Vec<_>>();

        assert_eq!(
            reviewed,
            vec![
                (40, "threads history mode", false),
                (41, "threads name", false),
                (42, "drop agent jobs", true),
                (43, "threads is pinned", false),
                (44, "external agent config imports provider id", false),
                (45, "threads section", false),
                (46, "threads section order", false),
            ]
        );

        for pair in CODEX_STATE_MIGRATIONS.windows(2) {
            assert_eq!(
                pair[1].version,
                pair[0].version + 1,
                "migration versions must stay contiguous: {} then {}",
                pair[0].version,
                pair[1].version
            );
        }
        for migration in CODEX_STATE_MIGRATIONS {
            assert!(
                !migration.description.is_empty(),
                "migration {} has an empty description",
                migration.version
            );
        }
    }

    /// Also the full-auto tripwire: this pins the COMPLETE breaking set, so
    /// an auto-synced migration that the classifier marks breaking turns the
    /// pipeline red for human review even though additive ones flow through.
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
        // Computed from the manifest so auto-synced additive migrations do
        // not re-pin this test; the breaking SET itself is pinned by
        // old_binary_missing_breaking_migration_is_degraded.
        let expected_additive = CODEX_STATE_MIGRATIONS
            .iter()
            .filter(|m| m.version > 35 && !m.breaking)
            .count();
        assert_eq!(a.additive().count(), expected_additive);
        let expected_breaking: Vec<u32> = CODEX_STATE_MIGRATIONS
            .iter()
            .filter(|m| m.version > 35 && m.breaking)
            .map(|m| m.version)
            .collect();
        assert!(expected_breaking.contains(&42));
        assert_eq!(
            a.breaking().map(|m| m.version).collect::<Vec<_>>(),
            expected_breaking
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

    // ----- served evidence -------------------------------------------------

    /// A served document whose manifest continues the compiled one by `extra`
    /// additive migrations, with the given observations.
    fn evidence_json(extra: u32, observed: &str) -> String {
        let mut migrations = compiled_manifest()
            .iter()
            .map(|m| {
                format!(
                    r#"{{"version":{},"description":{},"breaking":{}}}"#,
                    m.version,
                    serde_json::to_string(&m.description).unwrap(),
                    m.breaking
                )
            })
            .collect::<Vec<_>>();
        for n in 1..=extra {
            let version = compiled_max() + n;
            migrations.push(format!(
                r#"{{"version":{version},"description":"served migration {version}","breaking":false}}"#
            ));
        }
        format!(
            r#"{{"schema_version":1,"product":"codex","generated_at":"2026-08-20T21:20:16Z",
                "manifest":{{"source_ref":"rust-v9.9.9","migrations":[{}]}},
                "observed":[{observed}]}}"#,
            migrations.join(",")
        )
    }

    fn observation(db: u32, version: &str, verdict: &str, run: u64) -> String {
        format!(
            r#"{{"db_migration":{db},"version":"{version}","verdict":"{verdict}","runs":3,"run_number":{run},"observed_at":"2026-08-{run:02}T00:00:00Z","evidence":["why {verdict}"]}}"#
        )
    }

    #[test]
    fn evidence_that_is_absent_malformed_or_foreign_is_ignored() {
        let dir = tempdir().expect("tempdir");
        assert!(load_evidence(&dir.path().join("missing.json")).is_none());
        assert!(parse_evidence("not json").is_none());
        assert!(parse_evidence(r#"{"schema_version":2,"product":"codex"}"#).is_none());
        assert!(parse_evidence(r#"{"schema_version":1,"product":"claude"}"#).is_none());
        // Minimal valid document: nothing served, nothing observed.
        let evidence = parse_evidence(r#"{"schema_version":1,"product":"codex"}"#)
            .expect("minimal document is valid");
        assert_eq!(evidence, Evidence::default());
        assert_eq!(effective_manifest(Some(&evidence)), compiled_manifest());
    }

    #[test]
    fn served_manifest_extends_the_compiled_one_without_rewriting_it() {
        let mut evidence = parse_evidence(&evidence_json(2, "")).expect("valid");
        // A served entry that disagrees with compiled history is ignored, and
        // a gap in the served tail ends the extension.
        evidence.migrations[0].description = "rewritten history".into();
        evidence.migrations[0].breaking = true;
        let far = compiled_max() + 10;
        evidence.migrations.push(KnownMigration {
            version: far,
            description: "after a gap".into(),
            breaking: false,
        });

        let manifest = effective_manifest(Some(&evidence));
        assert_eq!(manifest[0], compiled_manifest()[0]);
        assert_eq!(manifest.len(), compiled_manifest().len() + 2);
        assert_eq!(manifest.last().map(|m| m.version), Some(compiled_max() + 2));
        assert!(manifest.iter().all(|m| m.version != far));
        for pair in manifest.windows(2) {
            assert_eq!(pair[1].version, pair[0].version + 1);
        }
    }

    #[test]
    fn served_manifest_turns_a_stale_guard_determinate() {
        let dir = tempdir().expect("tempdir");
        let future = compiled_max() + 1;
        state_db(
            dir.path(),
            &[
                (1, "threads", true),
                (future, &format!("served migration {future}"), true),
            ],
        );
        let binary = write(dir.path(), "codex", &["threads"]);

        // Compiled alone: the DB is ahead of what the guard knows.
        assert!(matches!(
            guard_in_dir(&binary, dir.path(), "rust-v0.1.0", None).outcome,
            AssessmentOutcome::Indeterminate(_)
        ));

        // Served: the extra migration is known and additive → assessed, safe.
        let evidence = parse_evidence(&evidence_json(1, "")).expect("valid");
        let result = guard_in_dir(&binary, dir.path(), "rust-v0.1.0", Some(&evidence));
        let AssessmentOutcome::Assessed(assessment) = &result.outcome else {
            panic!("expected assessed outcome, got {:?}", result.outcome);
        };
        assert!(!assessment.degraded());
        assert_eq!(assessment.db_max_applied, future);
        assert_eq!(
            result.manifest_source,
            ManifestSource::Served {
                source_ref: Some("rust-v9.9.9".into()),
                through: future
            }
        );
        assert_eq!(result.launch_verdict(), LaunchVerdict::Silent);
    }

    fn fixture_manifest() -> Vec<KnownMigration> {
        compiled_manifest()
    }

    fn evidence_with(observed: &[String]) -> Evidence {
        parse_evidence(&evidence_json(0, &observed.join(","))).expect("valid")
    }

    #[test]
    fn observed_compatible_outranks_a_static_degraded_verdict() {
        // Static: binary knows through 34, DB at manifest max → breaking 35/42.
        let evidence = evidence_with(&[observation(guard_max(), "rust-v0.10.0", "compatible", 90)]);
        let observed =
            find_observation(&evidence, &fixture_manifest(), guard_max(), "rust-v0.10.0")
                .expect("exact match");
        assert_eq!(observed.verdict, ObservedVerdict::Compatible);
        // The launch verdict goes quiet even though static says degraded.
        let applied: Vec<&str> = CODEX_STATE_MIGRATIONS
            .iter()
            .map(|m| m.description)
            .collect();
        let known: Vec<&str> = CODEX_STATE_MIGRATIONS
            .iter()
            .filter(|m| m.version <= 34)
            .map(|m| m.description)
            .collect();
        let assessment = assess_blobs(&applied, &known);
        assert!(assessment.degraded());
        let guard = Guard {
            outcome: AssessmentOutcome::Assessed(assessment),
            observed: Some(observed.clone()),
            manifest_source: ManifestSource::Compiled,
            evidence_generated_at: None,
        };
        assert_eq!(guard.launch_verdict(), LaunchVerdict::Silent);
    }

    #[test]
    fn observed_degraded_warns_even_when_static_is_clean() {
        let evidence = evidence_with(&[observation(guard_max(), "rust-v0.10.0", "degraded", 90)]);
        let observed =
            find_observation(&evidence, &fixture_manifest(), guard_max(), "rust-v0.10.0")
                .expect("exact match")
                .clone();
        let all: Vec<&str> = CODEX_STATE_MIGRATIONS
            .iter()
            .map(|m| m.description)
            .collect();
        let assessment = assess_blobs(&all, &all);
        assert!(!assessment.degraded());
        let guard = Guard {
            outcome: AssessmentOutcome::Assessed(assessment),
            observed: Some(observed.clone()),
            manifest_source: ManifestSource::Compiled,
            evidence_generated_at: None,
        };
        assert_eq!(
            guard.launch_verdict(),
            LaunchVerdict::ObservedDegraded(&observed)
        );
    }

    #[test]
    fn no_observation_falls_back_to_the_static_verdict() {
        let evidence = evidence_with(&[observation(guard_max(), "rust-v0.99.0", "compatible", 90)]);
        assert!(
            find_observation(&evidence, &fixture_manifest(), guard_max(), "rust-v0.10.0").is_none(),
            "another version's observation must not apply"
        );
        let applied: Vec<&str> = CODEX_STATE_MIGRATIONS
            .iter()
            .map(|m| m.description)
            .collect();
        let known: Vec<&str> = CODEX_STATE_MIGRATIONS
            .iter()
            .filter(|m| m.version <= 34)
            .map(|m| m.description)
            .collect();
        let assessment = assess_blobs(&applied, &known);
        let guard = Guard {
            outcome: AssessmentOutcome::Assessed(assessment),
            observed: None,
            manifest_source: ManifestSource::Compiled,
            evidence_generated_at: None,
        };
        assert!(matches!(
            guard.launch_verdict(),
            LaunchVerdict::StaticDegraded(_)
        ));
    }

    #[test]
    fn a_pessimistic_observation_reaches_back_through_additive_migrations_only() {
        let manifest = fixture_manifest();
        // 43..=max are all additive in the compiled manifest; 42 is breaking.
        assert!(manifest.iter().any(|m| m.version == 42 && m.breaking));
        assert!(manifest
            .iter()
            .filter(|m| m.version > 42)
            .all(|m| !m.breaking));
        let evidence = evidence_with(&[observation(guard_max(), "rust-v0.10.0", "degraded", 90)]);

        // DB at 43: every migration in 44..=max is additive → applies.
        assert!(find_observation(&evidence, &manifest, 43, "rust-v0.10.0").is_some());
        // DB at 41: migration 42 (breaking) lies in the gap → does not apply.
        assert!(find_observation(&evidence, &manifest, 41, "rust-v0.10.0").is_none());
        // DB beyond the observation: a newer DB is never covered by an older one.
        assert!(find_observation(&evidence, &manifest, guard_max() + 1, "rust-v0.10.0").is_none());
    }

    #[test]
    fn an_exact_observation_always_beats_a_newer_one_at_a_higher_migration() {
        // The ladder saw this version degrade at migration 46 (run 89) and
        // then pass at 48 and 50 (runs 90, 95). A user whose DB is still at 46
        // gets the exact measurement: a pass at 50 does not prove a pass at
        // 46, because "additive" only promises no schema removal, not that a
        // later migration did not repair what was wrong at 46.
        let manifest = fixture_manifest();
        let evidence = evidence_with(&[
            observation(46, "rust-v0.10.0", "degraded", 89),
            observation(48, "rust-v0.10.0", "compatible", 90),
            observation(50, "rust-v0.10.0", "compatible", 95),
        ]);
        let chosen = find_observation(&evidence, &manifest, 46, "rust-v0.10.0").expect("some");
        assert_eq!(chosen.db_migration, 46);
        assert_eq!(chosen.verdict, ObservedVerdict::Degraded);

        // Symmetric: an exact pass is not overruled by a later regression
        // observed at a higher migration.
        let evidence = evidence_with(&[
            observation(46, "rust-v0.10.0", "compatible", 89),
            observation(50, "rust-v0.10.0", "degraded", 95),
        ]);
        let chosen = find_observation(&evidence, &manifest, 46, "rust-v0.10.0").expect("some");
        assert_eq!(chosen.db_migration, 46);
        assert_eq!(chosen.verdict, ObservedVerdict::Compatible);

        // Several observations at the exact migration: the newest run wins.
        let evidence = evidence_with(&[
            observation(46, "rust-v0.10.0", "degraded", 89),
            observation(46, "rust-v0.10.0", "compatible", 91),
        ]);
        let chosen = find_observation(&evidence, &manifest, 46, "rust-v0.10.0").expect("some");
        assert_eq!(chosen.verdict, ObservedVerdict::Compatible);
    }

    #[test]
    fn without_an_exact_observation_only_a_pessimistic_reach_forward_applies() {
        let manifest = fixture_manifest();
        // DB at 47, no exact observation. A degraded result at the nearest
        // reachable migration (48) is worth warning about even though a newer
        // run at 50 passed — a pass up there proves nothing about 47.
        let evidence = evidence_with(&[
            observation(48, "rust-v0.10.0", "degraded", 90),
            observation(50, "rust-v0.10.0", "compatible", 95),
        ]);
        let chosen = find_observation(&evidence, &manifest, 47, "rust-v0.10.0").expect("some");
        assert_eq!(chosen.db_migration, 48);
        assert_eq!(chosen.verdict, ObservedVerdict::Degraded);

        // A compatible result up the additive chain is NEVER used to silence
        // the static verdict for this DB: migration 48 may be what repaired it.
        let evidence = evidence_with(&[observation(48, "rust-v0.10.0", "compatible", 90)]);
        assert!(find_observation(&evidence, &manifest, 47, "rust-v0.10.0").is_none());

        // Several runs at the nearest reachable migration: the newest decides,
        // and a cleared flap there is not reached for either.
        let evidence = evidence_with(&[
            observation(48, "rust-v0.10.0", "degraded", 90),
            observation(48, "rust-v0.10.0", "compatible", 91),
            observation(50, "rust-v0.10.0", "degraded", 95),
        ]);
        assert!(find_observation(&evidence, &manifest, 47, "rust-v0.10.0").is_none());

        // A breaking migration in the gap blocks the reach entirely.
        let evidence = evidence_with(&[observation(guard_max(), "rust-v0.10.0", "degraded", 95)]);
        assert!(find_observation(&evidence, &manifest, 41, "rust-v0.10.0").is_none());
    }

    /// A document that parses is still bounded in the work it can cause: the
    /// served manifest cannot grow the binary scan without limit, and an
    /// observation cannot carry megabytes onto stderr.
    #[test]
    fn a_parsed_document_is_bounded_in_work_and_output() {
        let first = compiled_max() + 1;
        let far = first as usize + EVIDENCE_MAX_SERVED_MIGRATIONS + 50;
        let migrations = (first as usize..=far)
            .map(|v| format!(r#"{{"version":{v},"description":"m{v}","breaking":false}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let long_description = "d".repeat(EVIDENCE_MAX_DESCRIPTION_BYTES + 1);
        let lines = (0..EVIDENCE_MAX_EVIDENCE_LINES + 5)
            .map(|i| format!(r#""{}""#, "x".repeat(EVIDENCE_MAX_LINE_CHARS + i)))
            .collect::<Vec<_>>()
            .join(",");
        let observations = (0..EVIDENCE_MAX_OBSERVATIONS + 10)
            .map(|i| format!(r#"{{"db_migration":{i},"version":"rust-v0.1.0","verdict":"degraded","evidence":[{lines}]}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let doc = format!(
            r#"{{"schema_version":1,"product":"codex","manifest":{{"migrations":[{migrations},
                {{"version":1,"description":"{long_description}","breaking":false}}]}},
                "observed":[{observations}]}}"#
        );
        let evidence = parse_evidence(&doc).expect("parses");
        assert_eq!(evidence.migrations.len(), EVIDENCE_MAX_SERVED_MIGRATIONS);
        assert_eq!(
            effective_manifest(Some(&evidence)).len(),
            compiled_manifest().len() + EVIDENCE_MAX_SERVED_MIGRATIONS
        );
        assert_eq!(evidence.observed.len(), EVIDENCE_MAX_OBSERVATIONS);
        let sample = &evidence.observed[0];
        assert_eq!(sample.evidence.len(), EVIDENCE_MAX_EVIDENCE_LINES);
        assert!(sample
            .evidence
            .iter()
            .all(|line| line.chars().count() <= EVIDENCE_MAX_LINE_CHARS));
    }

    /// The scan must give up, not hold a launch, when a served manifest makes
    /// it pathological: an expired deadline yields TimedOut, which the
    /// assessment reports as INDETERMINATE (silent at launch).
    #[test]
    fn a_binary_scan_past_its_budget_gives_up_as_indeterminate() {
        let dir = tempdir().expect("tempdir");
        // A binary that is all "t": every position matches the bucket of the
        // many "threads …" descriptions, so comparisons (work) pile up fast
        // even though the file is small — exactly the case an offset-based
        // check would miss.
        let binary = dir.path().join("codex");
        fs::write(&binary, vec![b't'; 64 * 1024]).unwrap();
        let manifest = compiled_manifest();
        let expired = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let error = versions_present_until(std::slice::from_ref(&binary), &manifest, expired)
            .expect_err("expired deadline stops the scan");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let binary = write(dir.path(), "codex-real", &["threads"]);
        // And a comfortable deadline still scans normally.
        let later = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let found = versions_present_until(std::slice::from_ref(&binary), &manifest, later)
            .expect("scan completes");
        assert!(found.contains(&1));
    }

    /// Observation lookup is linear in the document, not quadratic: a full
    /// cap's worth of same-version observations at the end of a long served
    /// tail resolves in well under a second.
    #[test]
    fn observation_lookup_is_bounded_under_a_full_document() {
        let served = EVIDENCE_MAX_SERVED_MIGRATIONS as u32;
        let far = compiled_max() + served;
        let observations = (0..EVIDENCE_MAX_OBSERVATIONS)
            .map(|_| observation(far, "rust-v0.10.0", "degraded", 95))
            .collect::<Vec<_>>()
            .join(",");
        let evidence = parse_evidence(&evidence_json(served, &observations)).expect("valid");
        let manifest = effective_manifest(Some(&evidence));
        assert_eq!(manifest.last().map(|m| m.version), Some(far));
        let started = std::time::Instant::now();
        let chosen =
            find_observation(&evidence, &manifest, compiled_max(), "rust-v0.10.0").expect("some");
        assert_eq!(chosen.db_migration, far);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn malformed_entries_and_mistyped_optional_fields_drop_not_reject() {
        let doc = format!(
            r#"{{"schema_version":1,"product":"codex","generated_at":123,
                "manifest":{{"source_ref":7,"migrations":[
                    {{"version":{},"description":"served next","breaking":false}},
                    {{"version":"x","description":"bad","breaking":false}},
                    {{"version":{},"description":"","breaking":false}}
                ]}},
                "observed":[
                    {{"db_migration":50,"version":"rust-v0.10.0","verdict":"compatible","runs":"3","evidence":"nope"}},
                    {{"db_migration":50}},
                    {{"db_migration":50,"version":"rust-v0.11.0","verdict":"mystery"}}
                ]}}"#,
            compiled_max() + 1,
            compiled_max() + 2
        );
        let evidence = parse_evidence(&doc).expect("document is still usable");
        assert_eq!(evidence.generated_at, None);
        assert_eq!(evidence.manifest_source_ref, None);
        assert_eq!(evidence.migrations.len(), 1);
        assert_eq!(evidence.migrations[0].version, compiled_max() + 1);
        assert_eq!(evidence.observed.len(), 1);
        assert_eq!(evidence.observed[0].runs, 0);
        assert!(evidence.observed[0].evidence.is_empty());
    }

    #[test]
    fn oversized_or_irregular_evidence_files_are_ignored() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join(EVIDENCE_FILE_NAME);
        std::fs::write(&path, evidence_json(0, "")).unwrap();
        assert!(load_evidence(&path).is_some(), "a normal file loads");
        // A directory at the path is not a regular file.
        assert!(load_evidence(dir.path()).is_none());
        // Past the size cap the file is not even read.
        let big = dir.path().join("big.json");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(EVIDENCE_MAX_BYTES + 1).unwrap();
        assert!(load_evidence(&big).is_none());
    }

    #[test]
    fn indeterminate_ledger_verdicts_are_not_evidence() {
        let evidence = evidence_with(&[observation(
            guard_max(),
            "rust-v0.10.0",
            "indeterminate",
            90,
        )]);
        assert!(evidence.observed.is_empty());
    }

    #[test]
    fn indeterminate_checks_stay_quiet_at_launch() {
        let guard = Guard {
            outcome: AssessmentOutcome::Indeterminate(Indeterminate {
                state_db: None,
                reason: "fixture".into(),
            }),
            observed: None,
            manifest_source: ManifestSource::Compiled,
            evidence_generated_at: None,
        };
        assert_eq!(guard.launch_verdict(), LaunchVerdict::Silent);
    }

    #[test]
    fn evidence_path_honours_the_registry_cache_env() {
        let dir = tempdir().expect("tempdir");
        std::env::set_var(REGISTRY_CACHE_ENV, dir.path());
        let path = default_evidence_path();
        std::env::remove_var(REGISTRY_CACHE_ENV);
        assert_eq!(path, Some(dir.path().join(EVIDENCE_FILE_NAME)));
    }
}
