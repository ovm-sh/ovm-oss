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
OVM_CARGO_TOML="crates/ovm/Cargo.toml"
scripts/bundle-manifest.sh validate "$BUNDLE_MANIFEST"
CURRENT=$(grep -m1 '^version = ' "$OVM_CARGO_TOML" | sed -E 's/version = "(.*)"/\1/')
echo "→ Current version: $CURRENT"

case "$1" in
    patch|minor|major)
        IFS='.' read -r MAJOR MINOR PATCH <<EOF
$CURRENT
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

# Keep every manifest-declared Cargo package on the same release version. Side
# binaries are published separately, but direct/Cargo updates require a coherent
# bundle for both stable and prerelease channels.
CARGO_TOMLS=""
while IFS= read -r package; do
    cargo_toml="crates/$package/Cargo.toml"
    if [ ! -f "$cargo_toml" ]; then
        echo "ERROR: manifest package '$package' has no $cargo_toml" >&2
        exit 1
    fi
    old=$(grep -m1 '^version = ' "$cargo_toml" | sed -E 's/version = "(.*)"/\1/')
    sed -i.bak -E "s/^version = \"$old\"/version = \"$NEW\"/" "$cargo_toml"
    rm -f "$cargo_toml.bak"
    CARGO_TOMLS="$CARGO_TOMLS $cargo_toml"
done <<EOF
$(scripts/bundle-manifest.sh packages "$BUNDLE_MANIFEST")
EOF

# Update Cargo.lock
cargo check --quiet >/dev/null 2>&1 || true

# Run the full pre-flight (formatting, clippy, tests).
echo "→ Running pre-flight checks..."
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --quiet

# Reminder to update CHANGELOG before commit
if ! grep -q "v$NEW\|## \[$NEW\]" CHANGELOG.md 2>/dev/null; then
    echo
    echo "WARNING: CHANGELOG.md has no entry for $NEW."
    printf "Open it now? (y/N) "
    read -r EDIT
    if [ "$EDIT" = "y" ] || [ "$EDIT" = "Y" ]; then
        ${EDITOR:-vi} CHANGELOG.md
    fi
fi

# Commit + tag
# Manifest validation restricts package-derived paths to safe crate names.
# shellcheck disable=SC2086
git add $CARGO_TOMLS Cargo.lock CHANGELOG.md 2>/dev/null || true
git commit -m "release: v$NEW"
git tag "v$NEW"

echo
echo "✓ Tagged v$NEW locally."
echo
echo "Next:"
echo "  git push origin main --tags"
echo
echo "That fires .github/workflows/release.yml — builds 4 platforms and creates"
echo "a GitHub Release. crates.io, npm, and Homebrew publish steps run only when"
echo "their tokens are configured."
