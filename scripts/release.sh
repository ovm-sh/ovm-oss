#!/bin/sh
# Cut a new release locally. Stops short of pushing — the user runs
# `git push origin main --tags` to fire the GitHub Actions release workflow.
#
# Usage:
#   ./scripts/release.sh patch          # 0.0.1 → 0.0.2
#   ./scripts/release.sh minor          # 0.0.1 → 0.1.0
#   ./scripts/release.sh major          # 0.0.1 → 1.0.0
#   ./scripts/release.sh 0.0.2          # explicit version
set -e

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

if [ -z "$1" ]; then
    echo "Usage: $0 <patch|minor|major|x.y.z>"
    exit 1
fi

# Sanity: clean tree + on main
if [ -n "$(git status --porcelain)" ]; then
    echo "ERROR: working tree is dirty. Commit or stash first."
    exit 1
fi
BRANCH=$(git rev-parse --abbrev-ref HEAD)
if [ "$BRANCH" != "main" ]; then
    echo "ERROR: not on main (current: $BRANCH)."
    exit 1
fi

BUNDLE_MANIFEST="crates/ovm/ovm-bundle-v1.tsv"
scripts/bundle-manifest.sh validate "$BUNDLE_MANIFEST"
metadata=$(cargo metadata --locked --no-deps --format-version 1)
CURRENT=$(printf '%s\n' "$metadata" | jq -er '.packages[] | select(.name == "ovm") | .version')
echo "→ Current version: $CURRENT"

case "$1" in
    patch|minor|major)
        IFS='.' read -r MAJOR MINOR PATCH <<EOF
${CURRENT%%-*}
EOF
        case "$1" in
            patch) PATCH=$((PATCH + 1));;
            minor) MINOR=$((MINOR + 1)); PATCH=0;;
            major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0;;
        esac
        NEW="${MAJOR}.${MINOR}.${PATCH}"
        ;;
    *.*.*)
        NEW="$1"
        ;;
    *)
        echo "ERROR: invalid bump '$1'. Use patch|minor|major or x.y.z."
        exit 1
        ;;
esac

echo "→ New version:     $NEW"
printf "Continue? (y/N) "
read -r ANSWER
if [ "$ANSWER" != "y" ] && [ "$ANSWER" != "Y" ]; then
    echo "Cancelled."
    exit 0
fi

# Every bundle crate inherits this one version. Restrict the edit to the
# workspace.package section so dependency versions can never be rewritten.
python3 - "$NEW" <<'PY_VERSION'
import pathlib
import re
import sys

version = sys.argv[1]
if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z][0-9A-Za-z.-]*)?", version):
    raise SystemExit(f"invalid release version: {version}")
manifest = pathlib.Path("Cargo.toml")
text = manifest.read_text()
pattern = r'(?ms)(^\[workspace\.package\]\s*\n(?:(?!^\[).)*?^version\s*=\s*)"[^"\n]+"'
updated, count = re.subn(pattern, lambda match: match[1] + '"' + version + '"', text)
if count != 1:
    raise SystemExit("expected one version in [workspace.package]")
manifest.write_text(updated)
PY_VERSION

# Refresh workspace versions in Cargo.lock without compiling or ignoring errors.
cargo update --workspace --offline

# Run the full pre-flight (formatting, clippy, tests).
echo "→ Running pre-flight checks..."
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --quiet

# Reminder to update CHANGELOG before commit. Only a release-section heading
# counts — link definitions or prose mentions of the version must not
# satisfy this check.
if ! grep -q "^## \[$NEW\]" CHANGELOG.md 2>/dev/null; then
    echo
    echo "WARNING: CHANGELOG.md has no entry for $NEW."
    printf "Open it now? (y/N) "
    read -r EDIT
    if [ "$EDIT" = "y" ] || [ "$EDIT" = "Y" ]; then
        ${EDITOR:-vi} CHANGELOG.md
    fi
fi

# Commit + tag
git commit -m "release: v$NEW" -- Cargo.toml Cargo.lock CHANGELOG.md
git tag "v$NEW"

echo
echo "✓ Tagged v$NEW locally."
echo
echo "Next:"
echo "  git push origin main --tags"
echo
echo "That fires .github/workflows/release.yml — builds 4 platforms and creates"
echo "a GitHub Release. Package registries and Homebrew are separate release paths."
