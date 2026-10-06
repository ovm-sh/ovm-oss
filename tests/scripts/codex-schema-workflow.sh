#!/usr/bin/env bash
# shellcheck disable=SC2016 # Contract literals intentionally contain shell syntax.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)

# Benchmark Deep is the only schema-detecting workflow since the hosted
# Benchmark Live lane was retired (2026-08-04). The file is absent in the
# exported public tree, so the contract only applies when it is present.
path="$ROOT/.github/workflows/benchmark-deep.yml"
if [[ -f "$path" ]]; then
  grep -Fq "bash scripts/sync-codex-migration-manifest.sh \"\$new_stable\"" "$path"
  grep -Fq 'cargo test -p ovm-codex-skew --locked' "$path"
  grep -Fq 'crates/ovm-codex-skew/src/lib.rs' "$path"
  # The manifest may only sync when the newest stable strictly ADVANCES: a
  # gate revocation legitimately regresses the registry max, and syncing to
  # the older tag would discard known-migration coverage (2026-07-28).
  grep -Fq 'key(new) > key(old)' "$path"
  grep -Fq 'if [ "$advanced" = "yes" ]; then' "$path"
  grep -Fq 'Newest stable regressed' "$path"
  # Always publish (2026-09-18): a breaking or unclassifiable migration is
  # flagged for review — dev log entry + issue — never held. The step must
  # not exit non-zero on a classification, and a failed sync must restore
  # the previous manifest and still publish.
  if grep -Fq 'if bad:' "$path"; then
    echo "FAIL: the schema step still exits on a classification"
    exit 1
  fi
  grep -Fq 'scripts/codex-schema-devlog.py' "$path"
  grep -Fq 'git checkout -- crates/ovm-codex-skew/src/lib.rs' "$path"
  grep -Fq '"kind": "manifest-sync-failed"' "$path"
  grep -Fq 'git add bench-data docs/api docs/devlog site' "$path"
fi

PYTHONDONTWRITEBYTECODE=1 python3 - "$ROOT" <<'PY'
import importlib.util
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
sys.path.insert(0, str(root / "scripts"))
from codex_schema import MigrationClassifier

# The detector is observatory-only, absent from the exported public tree.
detector = root / "scripts" / "detect-codex-schema-changes.py"
if detector.exists():
    spec = importlib.util.spec_from_file_location("detect_codex_schema_changes", detector)
    detect = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(detect)
    names = ["0055_thread_attachments.sql", "0056_next.sql"]
    assert detect.unreviewed(names, None) == names, "no manifest count: everything pending"
    assert detect.unreviewed(names, 54) == names, "manifest behind both: both pending"
    assert detect.unreviewed(names, 55) == ["0056_next.sql"], "manifest pins 55: only 56 pending"
    assert detect.unreviewed(names, 56) == [], "manifest pins both: nothing pending"
    assert detect.unreviewed(["unnumbered.sql"], 99) == ["unnumbered.sql"], "unnumbered files stay pending"

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
