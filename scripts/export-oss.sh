#!/usr/bin/env bash
# Stage the deliberate open-source surface into a new, empty directory.
set -euo pipefail

usage() {
  echo "Usage: $0 --source <repo> --ref <git-ref|WORKTREE> --destination <empty-dir>" >&2
  exit 2
}

SOURCE=
REF=
DESTINATION=
while [[ $# -gt 0 ]]; do
  case "$1" in
    --source)
      [[ $# -ge 2 ]] || usage
      SOURCE=$2
      shift 2
      ;;
    --ref)
      [[ $# -ge 2 ]] || usage
      REF=$2
      shift 2
      ;;
    --destination)
      [[ $# -ge 2 ]] || usage
      DESTINATION=$2
      shift 2
      ;;
    *) usage ;;
  esac
done

[[ -n "$SOURCE" && -n "$REF" && -n "$DESTINATION" ]] || usage
SOURCE=$(cd "$SOURCE" && pwd)
git -C "$SOURCE" rev-parse --show-toplevel >/dev/null

if [[ -e "$DESTINATION" ]]; then
  [[ -d "$DESTINATION" ]] || {
    echo "destination is not a directory: $DESTINATION" >&2
    exit 1
  }
  [[ -z "$(find "$DESTINATION" -mindepth 1 -maxdepth 1 -print -quit)" ]] || {
    echo "destination must be empty: $DESTINATION" >&2
    exit 1
  }
else
  mkdir -p "$DESTINATION"
fi
DESTINATION=$(cd "$DESTINATION" && pwd)

# This is intentionally a small, product-oriented allowlist. Research data,
# deployment state, private automation, and maintainer notes are absent unless
# explicitly promoted here.
PUBLIC_PATHS=(
  .coderabbit.yaml
  .gitignore
  .gitleaks.toml
  .hooks
  CHANGELOG.md
  CLAUDE.md
  CODE_OF_CONDUCT.md
  CONTRIBUTING.md
  Cargo.lock
  Cargo.toml
  LICENSE
  README.md
  RELEASING.md
  SECURITY.md
  crates/ovm
  crates/ovm-claudex
  crates/ovm-codex-skew
  deny.toml
  docs/api
  docs/architecture.md
  docs/features
  docs/fork-build-import.md
  install.sh
  npm
  scripts/bundle-manifest.sh
  scripts/dev-install.sh
  scripts/dev-uninstall.sh
  scripts/export-oss.sh
  scripts/codex_schema.py
  scripts/gen-codex-migration-manifest.py
  scripts/oss-templates
  scripts/publish-npm.sh
  scripts/release.sh
  scripts/sync-codex-migration-manifest.sh
  scripts/update-brew-formula.sh
  scripts/update-registry.sh
  scripts/version-canary-test.sh
  tests/compatibility
  tests/scripts/bundle-contract.sh
  tests/scripts/bundle-manifest.sh
  tests/scripts/canary-missing-version.sh
  tests/scripts/codex-migration-manifest.sh
  tests/scripts/first-run-acceptance.sh
  tests/scripts/install-path-wiring.sh
  tests/scripts/codex-schema-workflow.sh
  tests/scripts/e2e-command-matrix.sh
  tests/scripts/export-oss.sh
  tests/scripts/npm-install-transactional.sh
  tests/scripts/npm-package-map.sh
  tests/scripts/sync-codex-migration-manifest.sh
  tests/scripts/syntax-check.sh
  tests/scripts/update-registry-smoke.sh
)

REQUIRED_PUBLIC_FILES=(
  Cargo.lock
  Cargo.toml
  crates/ovm/ovm-bundle-v1.tsv
  scripts/oss-templates/ci.yml
  scripts/oss-templates/release.yml
)

if [[ "$REF" == WORKTREE ]]; then
  for path in "${PUBLIC_PATHS[@]}"; do
    [[ -e "$SOURCE/$path" ]] || {
      echo "public export path is missing: $path" >&2
      exit 1
    }
  done
  TRACKED_PATHS=()
  while IFS= read -r path; do
    TRACKED_PATHS+=("$path")
  done < <(git -C "$SOURCE" ls-files -- "${PUBLIC_PATHS[@]}")
  [[ ${#TRACKED_PATHS[@]} -gt 0 ]] || {
    echo "public export contains no tracked files" >&2
    exit 1
  }
  tar -C "$SOURCE" -cf - "${TRACKED_PATHS[@]}" | tar -C "$DESTINATION" -xf -
else
  git -C "$SOURCE" rev-parse --verify "${REF}^{commit}" >/dev/null
  for path in "${REQUIRED_PUBLIC_FILES[@]}"; do
    git -C "$SOURCE" cat-file -e "$REF:$path" 2>/dev/null || {
      echo "required public export file is missing from $REF: $path" >&2
      exit 1
    }
  done
  git -C "$SOURCE" archive --format=tar "$REF" -- "${PUBLIC_PATHS[@]}" |
    tar -C "$DESTINATION" -xf -
fi

if find "$DESTINATION" -type l -print -quit | grep -q .; then
  echo "public export contains a symlink" >&2
  exit 1
fi

mkdir -p "$DESTINATION/.github/workflows"
cp "$DESTINATION/scripts/oss-templates/ci.yml" \
  "$DESTINATION/.github/workflows/ci.yml"
cp "$DESTINATION/scripts/oss-templates/release.yml" \
  "$DESTINATION/.github/workflows/release.yml"
cp "$DESTINATION/scripts/oss-templates/CLAUDE.md" "$DESTINATION/CLAUDE.md"
cp "$DESTINATION/scripts/oss-templates/RELEASING.md" "$DESTINATION/RELEASING.md"

sanitize_oss_file() {
  local path=$1
  local output="${path}.oss-export"
  local was_executable=0
  [[ -x "$path" ]] && was_executable=1
  awk '
    /^# OSS-OMIT-BEGIN$/ { omit = 1; next }
    /^# OSS-OMIT-END$/ { omit = 0; next }
    !omit { print }
  ' "$path" > "$output"
  if (( was_executable )); then
    chmod +x "$output"
  fi
  mv "$output" "$path"
}

sanitize_oss_file "$DESTINATION/.gitleaks.toml"
sanitize_oss_file "$DESTINATION/scripts/dev-install.sh"
sanitize_oss_file "$DESTINATION/scripts/dev-uninstall.sh"

echo "staged OSS tree from $REF at $DESTINATION"
