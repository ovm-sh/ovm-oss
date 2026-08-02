#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

MIGRATIONS="$TMP_DIR/migrations"
TARGET="$TMP_DIR/lib.rs"
mkdir -p "$MIGRATIONS"
printf '%s\n' 'CREATE TABLE threads (id TEXT, legacy TEXT);' > "$MIGRATIONS/0001_threads.sql"
printf '%s\n' 'DROP TABLE IF EXISTS agent_jobs;' > "$MIGRATIONS/0002_drop_agent_jobs.sql"
cat > "$MIGRATIONS/0003_rebuild_threads.sql" <<'SQL'
CREATE TABLE threads_new (id TEXT);
INSERT INTO threads_new (id) SELECT id FROM threads;
DROP TABLE threads;
ALTER TABLE threads_new RENAME TO threads;
SQL
HOSTILE_FILENAME=$'0004_quote"slash\\\\line\npub const FILENAME_INJECTION: bool = true;.sql'
printf '%s\n' 'CREATE TABLE harmless (id TEXT);' > "$MIGRATIONS/$HOSTILE_FILENAME"
cat > "$MIGRATIONS/0005_hostile_table.sql" <<'SQL'
CREATE TABLE "payload""\slash
pub const SQL_INJECTION: bool = true;" (id TEXT);
SQL
cat > "$MIGRATIONS/0006_drop_hostile_table.sql" <<'SQL'
DROP TABLE "payload""\slash
pub const SQL_INJECTION: bool = true;";
SQL
printf '%s\n' \
  '#![allow(dead_code)]' \
  'struct Migration {' \
  '    version: u32,' \
  "    description: &'static str," \
  '    breaking: bool,' \
  '}' \
  '// CODEX_STATE_MIGRATIONS_BEGIN' \
  'stale manifest' \
  '// CODEX_STATE_MIGRATIONS_END' \
  'fn after() {}' > "$TARGET"

if python3 "$ROOT/scripts/gen-codex-migration-manifest.py" \
  "$MIGRATIONS" --source-ref rust-v9.9.9 --check "$TARGET"
then
  echo "stale manifest unexpectedly passed --check" >&2
  exit 1
fi

python3 "$ROOT/scripts/gen-codex-migration-manifest.py" \
  "$MIGRATIONS" --source-ref rust-v9.9.9 --write "$TARGET"

grep -Fq 'at rust-v9.9.9' "$TARGET"
grep -Fq 'Migration { version: 1, description: "threads", breaking: false },' "$TARGET"
grep -Fq 'Migration { version: 2, description: "drop agent jobs", breaking: true }, // removes agent_jobs' "$TARGET"
grep -Fq 'Migration { version: 3, description: "rebuild threads", breaking: true }, // removes threads.legacy' "$TARGET"
grep -Fq 'description: "quote\"slash\\\\line\npub const FILENAME INJECTION: bool = true;"' "$TARGET"
grep -Fq '// removes payload, payload\"\\slash\npub const SQL_INJECTION: bool = true;' "$TARGET"
if grep -Eq '^[[:space:]]*pub const (FILENAME|SQL)_INJECTION' "$TARGET"
then
  echo "hostile upstream text escaped its generated Rust context" >&2
  exit 1
fi
grep -Fq '#![allow(dead_code)]' "$TARGET"
grep -Fq 'after' "$TARGET"
rustc --edition 2021 --crate-type lib "$TARGET" -o "$TMP_DIR/libmanifest.rlib"

python3 "$ROOT/scripts/gen-codex-migration-manifest.py" \
  "$MIGRATIONS" --source-ref rust-v9.9.9 --check "$TARGET"

UNREPLAYABLE="$TMP_DIR/unreplayable"
mkdir -p "$UNREPLAYABLE"
printf '%s\n' 'CREATE TABLE threads (id TEXT);' > "$UNREPLAYABLE/0001_threads.sql"
printf '%s\n' \
  'ALTER TABLE threads RENAME COLUMN missing TO renamed;' \
  > "$UNREPLAYABLE/0002_failed_rename.sql"
if python3 "$ROOT/scripts/gen-codex-migration-manifest.py" \
  "$UNREPLAYABLE" --source-ref rust-v9.9.9 >/dev/null
then
  echo "unreplayable migration was rendered false-safe" >&2
  exit 1
fi

# The manifest may only grow. Generation rewrites the whole block from one
# upstream tag, so if a later tag dropped or reworded a historical migration a
# plain overwrite would silently discard coverage — and losing a `breaking`
# entry turns a real downgrade hazard into a clean "compatible" verdict.
SHRUNK="$TMP_DIR/shrunk"
mkdir -p "$SHRUNK"
cp "$MIGRATIONS"/0001_*.sql "$SHRUNK/"
if python3 "$ROOT/scripts/gen-codex-migration-manifest.py" \
  "$SHRUNK" --source-ref rust-v9.9.9 --write "$TARGET" 2>"$TMP_DIR/shrink.err"
then
  echo "a shrinking manifest was accepted" >&2
  exit 1
fi
grep -Fq 'may only grow' "$TMP_DIR/shrink.err" || {
  echo "shrink was rejected for the wrong reason:" >&2
  cat "$TMP_DIR/shrink.err" >&2
  exit 1
}

# Rewording a known migration is equally unsafe: descriptions are the identity
# the guard matches against the DB's own migration table.
REWORDED="$TMP_DIR/reworded"
mkdir -p "$REWORDED"
cp "$MIGRATIONS"/*.sql "$REWORDED/"
mv "$REWORDED"/0001_*.sql "$REWORDED/0001_threads_renamed.sql"
if python3 "$ROOT/scripts/gen-codex-migration-manifest.py" \
  "$REWORDED" --source-ref rust-v9.9.9 --write "$TARGET" 2>"$TMP_DIR/reword.err"
then
  echo "a reworded migration was accepted" >&2
  exit 1
fi
grep -Fq 'refusing to' "$TMP_DIR/reword.err" || {
  echo "reword was rejected for the wrong reason:" >&2
  cat "$TMP_DIR/reword.err" >&2
  exit 1
}

echo "codex-migration-manifest: ok"
