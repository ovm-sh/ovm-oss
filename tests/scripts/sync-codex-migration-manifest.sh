#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

UPSTREAM="$TMP_DIR/codex"
MIGRATIONS="$UPSTREAM/codex-rs/state/migrations"
TARGET="$TMP_DIR/lib.rs"
mkdir -p "$MIGRATIONS"
printf '%s\n' \
  'CREATE TABLE threads (id TEXT);' \
  'CREATE TABLE agent_jobs (id TEXT);' \
  > "$MIGRATIONS/0001_threads.sql"
printf '%s\n' 'ALTER TABLE threads ADD COLUMN name TEXT;' > "$MIGRATIONS/0002_threads_name.sql"
printf '%s\n' 'DROP TABLE agent_jobs;' > "$MIGRATIONS/0003_drop_agent_jobs.sql"
git -C "$UPSTREAM" init -q
git -C "$UPSTREAM" add codex-rs/state/migrations
git -C "$UPSTREAM" -c user.name=test -c user.email=test@example.invalid \
  commit -qm 'fixture migrations'
fixture_commit=$(git -C "$UPSTREAM" rev-parse HEAD)
git -C "$UPSTREAM" tag rust-v1.2.3

printf '%s\n' \
  '// CODEX_STATE_MIGRATIONS_BEGIN' \
  'stale' \
  '// CODEX_STATE_MIGRATIONS_END' > "$TARGET"

CODEX_SOURCE_URL="$UPSTREAM" CODEX_MIGRATION_TARGET="$TARGET" \
  bash "$ROOT/scripts/sync-codex-migration-manifest.sh" rust-v1.2.3

grep -Fq 'at rust-v1.2.3' "$TARGET"
grep -Fq "source commit: $fixture_commit" "$TARGET"
grep -Fq 'Migration { version: 3, description: "drop agent jobs", breaking: true }, // removes agent_jobs' "$TARGET"

if CODEX_SOURCE_URL="$UPSTREAM" CODEX_MIGRATION_TARGET="$TARGET" \
  bash "$ROOT/scripts/sync-codex-migration-manifest.sh" not-a-release
then
  echo "sync accepted an invalid Codex release tag" >&2
  exit 1
fi

echo "sync-codex-migration-manifest: ok"
