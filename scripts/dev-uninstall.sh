#!/bin/sh
# Remove only checkout-bound links from OVM's retired developer workflow.
# Standalone self-managed snapshots are intentionally preserved.
set -eu

REPO_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
ARTIFACT_DIR="$REPO_ROOT/target/release"
INSTALL_DIR="${OVM_INSTALL_DIR:-$HOME/.ovm/bin}"
CARGO_BIN="$HOME/.cargo/bin"
removed=0

remove_exact_link() {
    link=$1
    expected=$2
    if [ -L "$link" ] && [ "$(readlink "$link")" = "$expected" ]; then
        rm "$link"
        echo "✓ Removed legacy link: $link"
        removed=$((removed + 1))
    elif [ -e "$link" ] || [ -L "$link" ]; then
        echo "Preserving non-legacy path: $link"
    fi
}

remove_exact_link "$CARGO_BIN/ovm" "$ARTIFACT_DIR/ovm"
remove_exact_link "$CARGO_BIN/ovm-claudex" "$ARTIFACT_DIR/ovm-claudex"
remove_exact_link "$INSTALL_DIR/ovm" "$ARTIFACT_DIR/ovm"
remove_exact_link "$INSTALL_DIR/ovm-codex-skew" "$ARTIFACT_DIR/ovm-codex-skew"
remove_exact_link "$INSTALL_DIR/ovm-diff" "$REPO_ROOT/plugins/diff/ovm-diff"

for product in claude codex pi; do
    remove_exact_link "$INSTALL_DIR/$product" "$ARTIFACT_DIR/ovm"
done

if [ "$removed" -eq 0 ]; then
    echo "No checkout-bound OVM links were found."
fi

echo "Standalone versions under ~/.ovm/self are untouched."
echo "Use 'ovm self list' and 'ovm self use <version>' to manage them."
