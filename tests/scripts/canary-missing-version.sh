#!/usr/bin/env bash
# Smoke test: scripts/version-canary-test.sh must emit valid JSON even when
# the requested version has no installed binary. The canary workflow parses
# this JSON to decide whether to file a regression issue, so a broken script
# silently flips us back into the failing state the user just dug us out of.
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo_root"

# Run against a guaranteed-missing version. Use a throwaway HOME so we don't
# read anything the host may have installed.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

HOME="$tmp" zsh scripts/version-canary-test.sh 99.99.99-canary-smoke \
    > "$tmp/out.json" 2> "$tmp/err.log" || true

cat "$tmp/err.log" >&2
cat "$tmp/out.json"

python3 - "$tmp/out.json" <<'PY'
import json, sys
path = sys.argv[1]
with open(path) as f:
    data = json.load(f)

assert data["version"] == "99.99.99-canary-smoke", data
assert data["overall"] == "fail", data
names = {t["name"]: t["status"] for t in data["tests"]}
assert names.get("binary_exists") == "fail", f"binary_exists not fail: {names}"
for skipped in ("version_output", "help_output", "buddy_command"):
    assert names.get(skipped) == "skip", f"{skipped} not skip: {names}"
print("canary-missing-version: ok")
PY
