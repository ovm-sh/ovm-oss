#!/bin/sh
# Build and install a standalone, content-addressed OVM development snapshot.
# The installed commands do not retain symlinks into this checkout.
set -eu

REPO_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
MANIFEST="${OVM_BUNDLE_MANIFEST:-$REPO_ROOT/crates/ovm/ovm-bundle-v1.tsv}"
ARTIFACT_DIR="${OVM_DEV_ARTIFACT_DIR:-$REPO_ROOT/target/release}"
INSTALL_DIR="${OVM_INSTALL_DIR:-$HOME/.ovm/bin}"
CARGO_BIN="$HOME/.cargo/bin"
CARGO_COMMAND="${CARGO:-cargo}"
SKIP_BUILD="${OVM_DEV_SKIP_BUILD:-0}"
LEGACY_ROOT="${OVM_LEGACY_ROOT:-$REPO_ROOT}"

sha256_stream() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 | awk '{print $1}'
    else
        echo "ERROR: sha256sum or shasum is required" >&2
        exit 1
    fi
}

replace_path() {
    source=$1
    destination=$2
    case "$(uname -s)" in
        Darwin) mv -fh "$source" "$destination" ;;
        *) mv -Tf "$source" "$destination" ;;
    esac
}

switch_link() {
    link=$1
    target=$2
    if [ -e "$link" ] && [ ! -L "$link" ]; then
        echo "ERROR: refusing to replace non-symlink at $link" >&2
        exit 1
    fi
    parent=$(dirname "$link")
    temp="$parent/.ovm-dev-link-$$"
    rm -f "$temp"
    ln -s "$target" "$temp"
    replace_path "$temp" "$link"
}

atomic_copy() {
    source=$1
    destination=$2
    parent=$(dirname "$destination")
    temp=$(mktemp "$parent/.ovm-dev-copy.XXXXXX")
    cp "$source" "$temp"
    chmod 755 "$temp"
    replace_path "$temp" "$destination"
}

legacy_link_to() {
    link=$1
    expected=$2
    [ -L "$link" ] && [ "$(readlink "$link")" = "$expected" ]
}

"$REPO_ROOT/scripts/bundle-manifest.sh" validate "$MANIFEST"

if [ "$SKIP_BUILD" = "1" ]; then
    echo "→ Using prebuilt development artifacts..."
else
    echo "→ Building release workspace..."
    (cd "$REPO_ROOT" && "$CARGO_COMMAND" build --release --locked)
fi

while IFS= read -r binary; do
    [ -x "$ARTIFACT_DIR/$binary" ] || {
        echo "ERROR: build did not produce $ARTIFACT_DIR/$binary" >&2
        exit 1
    }
done <<EOF
$("$REPO_ROOT/scripts/bundle-manifest.sh" binaries "$MANIFEST")
EOF

content_hash=$(
    {
        sha256_stream < "$MANIFEST"
        while IFS= read -r binary; do
            sha256_stream < "$ARTIFACT_DIR/$binary"
        done <<EOF
$("$REPO_ROOT/scripts/bundle-manifest.sh" binaries "$MANIFEST")
EOF
    } | sha256_stream
)
version="dev-$(printf '%s' "$content_hash" | cut -c1-16)"

echo "→ Installing standalone snapshot $version..."
if ! OVM_LOCAL_ARTIFACT_DIR="$ARTIFACT_DIR" \
    OVM_LOCAL_MANIFEST="$MANIFEST" \
    OVM_LOCAL_VERSION="$version" \
    OVM_LEGACY_ROOT="$LEGACY_ROOT" \
    OVM_INSTALL_DIR="$INSTALL_DIR" \
    sh "$REPO_ROOT/install.sh"; then
    exit 1
fi

mkdir -p "$INSTALL_DIR"
INSTALL_DIR_REAL=$(CDPATH='' cd "$INSTALL_DIR" && pwd -P)

# Preserve the private plugin as a standalone copy. It is deliberately absent
# from the public bundle manifest and every release package.
PRIVATE_PLUGIN="${OVM_DEV_PRIVATE_PLUGIN:-$REPO_ROOT/plugins/diff/ovm-diff}"
PRIVATE_DEST="$INSTALL_DIR/ovm-diff"
if [ -x "$PRIVATE_PLUGIN" ]; then
    if [ -L "$PRIVATE_DEST" ] && ! legacy_link_to "$PRIVATE_DEST" "$LEGACY_ROOT/plugins/diff/ovm-diff"; then
        echo "WARNING: preserving foreign ovm-diff symlink at $PRIVATE_DEST" >&2
    elif [ -d "$PRIVATE_DEST" ]; then
        echo "WARNING: preserving directory at $PRIVATE_DEST" >&2
    else
        atomic_copy "$PRIVATE_PLUGIN" "$PRIVATE_DEST"
        echo "  Copied private plugin: $PRIVATE_DEST"
    fi
fi

# Existing managed launchers may still pin this checkout. Rewire only exact
# legacy links; preserve regular files and unrelated symlinks.
for product in claude codex pi; do
    launcher="$INSTALL_DIR/$product"
    if legacy_link_to "$launcher" "$LEGACY_ROOT/target/release/ovm"; then
        switch_link "$launcher" ovm
        echo "  Migrated launcher: $launcher → ovm"
    elif [ -L "$launcher" ]; then
        target=$(readlink "$launcher")
        case "$target" in
            ovm|"$INSTALL_DIR/ovm"|"$INSTALL_DIR_REAL/ovm") ;;
            *) echo "WARNING: preserving foreign $product launcher → $target" >&2 ;;
        esac
    fi
done

# Remove only the checkout symlinks created by the old developer workflow.
for name in ovm ovm-claudex; do
    link="$CARGO_BIN/$name"
    if legacy_link_to "$link" "$LEGACY_ROOT/target/release/$name"; then
        rm "$link"
        echo "  Removed legacy Cargo link: $link"
    elif [ -e "$link" ] || [ -L "$link" ]; then
        echo "WARNING: preserving non-legacy Cargo install at $link" >&2
    fi
done

for path in "$INSTALL_DIR/ovm" "$INSTALL_DIR/ovm-codex-skew" "$INSTALL_DIR/ovm-claudex"; do
    if [ -L "$path" ]; then
        target=$(readlink "$path")
        case "$target" in
            "$REPO_ROOT"/*)
                echo "ERROR: installed path still points into checkout: $path → $target" >&2
                exit 1
                ;;
        esac
    fi
done

active=$(command -v ovm 2>/dev/null || true)
case "$active" in
    "$INSTALL_DIR/ovm"|"") ;;
    *) echo "WARNING: $active currently shadows $INSTALL_DIR/ovm on PATH" >&2 ;;
esac

echo ""
echo "✓ Installed standalone OVM snapshot $version"
echo "  The checkout can move without breaking the installed commands."
echo "  Rerun ./scripts/dev-install.sh after code changes."
