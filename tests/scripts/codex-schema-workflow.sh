#!/usr/bin/env bash
# shellcheck disable=SC2016 # Contract literals intentionally contain shell syntax.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)

if [[ -f "$ROOT/.github/workflows/benchmark-live.yml" ||
  -f "$ROOT/.github/workflows/benchmark-deep.yml" ]]; then
  for workflow in benchmark-live.yml benchmark-deep.yml; do
    path="$ROOT/.github/workflows/$workflow"
    [[ -f "$path" ]] || {
      echo "private schema workflow is missing: $path" >&2
      exit 1
    }
    grep -Fq "bash scripts/sync-codex-migration-manifest.sh \"\$new_stable\"" "$path"
    grep -Fq 'cargo test -p ovm-codex-skew --locked' "$path"
    grep -Fq 'crates/ovm-codex-skew/src/lib.rs' "$path"
    # The manifest may only sync when the newest stable strictly ADVANCES: a
    # gate revocation legitimately regresses the registry max, and syncing to
    # the older tag would discard known-migration coverage (2026-07-28).
    grep -Fq 'key(new) > key(old)' "$path"
    grep -Fq 'if [ "$advanced" = "yes" ]; then' "$path"
    grep -Fq 'Newest stable regressed' "$path"
  done
fi

PYTHONDONTWRITEBYTECODE=1 python3 - "$ROOT" <<'PY'
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
sys.path.insert(0, str(root / "scripts"))
from codex_schema import MigrationClassifier

prior = "CREATE TABLE threads (id TEXT, legacy TEXT);"
rebuild = """
CREATE TABLE threads_new (id TEXT);
INSERT INTO threads_new (id) SELECT id FROM threads;
DROP TABLE threads;
ALTER TABLE threads_new RENAME TO threads;
"""
classifier = MigrationClassifier()
assert not classifier.classify(prior).breaking
assert classifier.classify(rebuild).breaking, "column-dropping rebuild was false-safe"

classifier = MigrationClassifier()
assert not classifier.classify("CREATE TABLE threads (id TEXT);").indeterminate
failed_rename = classifier.classify(
    "ALTER TABLE threads RENAME COLUMN missing TO renamed;"
)
assert failed_rename.indeterminate, "failed migration replay was false-safe"
PY

echo "codex-schema-workflow: ok"
