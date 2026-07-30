#!/usr/bin/env bash
# Refresh ovm-codex-skew from one exact upstream Codex release tag.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TAG=${1:-}
SOURCE_URL=${CODEX_SOURCE_URL:-https://github.com/openai/codex.git}
TARGET=${CODEX_MIGRATION_TARGET:-$ROOT/crates/ovm-codex-skew/src/lib.rs}

if [[ ! "$TAG" =~ ^rust-v[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]; then
  echo "ERROR: expected an exact Codex release tag, got: ${TAG:-<empty>}" >&2
  exit 2
fi

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

git clone --quiet --depth 1 --branch "$TAG" "$SOURCE_URL" "$TMP_DIR/codex"

actual_commit=$(git -C "$TMP_DIR/codex" rev-parse HEAD)
remote_refs=$(git ls-remote "$SOURCE_URL" "refs/tags/$TAG" "refs/tags/$TAG^{}")
expected_commit=$(printf '%s\n' "$remote_refs" | awk '/\^\{\}$/ { print $1; exit }')
if [[ -z "$expected_commit" ]]; then
  expected_commit=$(printf '%s\n' "$remote_refs" | awk 'NR == 1 { print $1 }')
fi
if [[ -z "$expected_commit" || "$actual_commit" != "$expected_commit" ]]; then
  echo "ERROR: checked-out Codex commit does not match remote tag $TAG" >&2
  exit 1
fi

python3 "$ROOT/scripts/gen-codex-migration-manifest.py" \
  "$TMP_DIR/codex/codex-rs/state/migrations" \
  --source-ref "$TAG" \
  --source-commit "$actual_commit" \
  --write "$TARGET"

python3 "$ROOT/scripts/gen-codex-migration-manifest.py" \
  "$TMP_DIR/codex/codex-rs/state/migrations" \
  --source-ref "$TAG" \
  --source-commit "$actual_commit" \
  --check "$TARGET"

echo "Codex migration manifest synced from $TAG ($actual_commit)"
