#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

DEST="$TMP_DIR/export"
bash "$ROOT/scripts/export-oss.sh" --source "$ROOT" --ref WORKTREE --destination "$DEST"

for required in \
  Cargo.toml \
  crates/ovm/src/main.rs \
  crates/ovm-claudex/src/main.rs \
  crates/ovm-codex-skew/src/lib.rs \
  .github/workflows/ci.yml \
  .github/workflows/release.yml \
  scripts/export-oss.sh \
  scripts/codex_schema.py \
  scripts/gen-codex-migration-manifest.py \
  scripts/sync-codex-migration-manifest.sh \
  tests/scripts/codex-migration-manifest.sh \
  tests/scripts/codex-schema-workflow.sh \
  tests/scripts/e2e-command-matrix.sh \
  tests/scripts/sync-codex-migration-manifest.sh \
  scripts/update-registry.sh \
  tests/scripts/export-oss.sh
do
  [[ -f "$DEST/$required" ]] || {
    echo "missing exported OSS file: $required" >&2
    exit 1
  }
done

for executable in \
  scripts/dev-install.sh \
  scripts/dev-uninstall.sh
do
  [[ -x "$DEST/$executable" ]] || {
    echo "exported OSS executable lost its mode: $executable" >&2
    exit 1
  }
done

for private_path in \
  bench-data \
  site \
  tools/benchmark \
  .github/workflows/benchmark-live.yml \
  docs/public-release-prep.md \
  tests/scripts/public-ci-exported-tree.sh
do
  [[ ! -e "$DEST/$private_path" ]] || {
    echo "private-only path escaped into OSS export: $private_path" >&2
    exit 1
  }
done

cmp "$ROOT/scripts/oss-templates/ci.yml" "$DEST/.github/workflows/ci.yml"
cmp "$ROOT/scripts/oss-templates/release.yml" "$DEST/.github/workflows/release.yml"
cmp "$ROOT/scripts/oss-templates/CLAUDE.md" "$DEST/CLAUDE.md"
cmp "$ROOT/scripts/oss-templates/RELEASING.md" "$DEST/RELEASING.md"

# Exercise the exact schema contract invoked by the exported CI, from inside
# the exported tree where private workflows and detector automation are absent.
(cd "$DEST" && bash tests/scripts/codex-schema-workflow.sh)

if grep -R -n 'OSS-OMIT' \
  "$DEST/.gitleaks.toml" "$DEST/scripts/dev-install.sh" "$DEST/scripts/dev-uninstall.sh"
then
  echo "private-only marked content escaped into OSS export" >&2
  exit 1
fi

while IFS= read -r exported; do
  relative=${exported#"$DEST/"}
  git -C "$ROOT" ls-files --error-unmatch "$relative" >/dev/null || {
    echo "exported path is not tracked: $relative" >&2
    exit 1
  }
done < <(find "$DEST" -type f -print)

printf 'do not overwrite\n' > "$TMP_DIR/not-empty"
if bash "$ROOT/scripts/export-oss.sh" \
  --source "$ROOT" \
  --ref WORKTREE \
  --destination "$TMP_DIR" 2>/dev/null
then
  echo "export accepted a non-empty destination" >&2
  exit 1
fi

if find "$DEST" -type l -print -quit | grep -q .; then
  echo "OSS export contains a symlink" >&2
  exit 1
fi

FIXTURE="$TMP_DIR/source-without-manifest"
git clone --quiet --shared "$ROOT" "$FIXTURE"
git -C "$FIXTURE" rm --quiet crates/ovm/ovm-bundle-v1.tsv
git -C "$FIXTURE" -c user.name=test -c user.email=test@example.invalid \
  commit --quiet -m "test: remove required manifest"
if bash "$ROOT/scripts/export-oss.sh" \
  --source "$FIXTURE" \
  --ref HEAD \
  --destination "$TMP_DIR/missing-manifest-export" 2>/dev/null
then
  echo "export accepted a tag tree without the required bundle manifest" >&2
  exit 1
fi

echo "export-oss: ok"
