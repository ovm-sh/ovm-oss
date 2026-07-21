#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
HELPER="$ROOT/scripts/bundle-manifest.sh"
MANIFEST="$ROOT/crates/ovm/ovm-bundle-v1.tsv"
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

"$HELPER" validate "$MANIFEST"
binaries=$("$HELPER" binaries "$MANIFEST" | tr '\n' ' ' | sed 's/ $//')
[[ "$binaries" == "ovm ovm-codex-skew ovm-claudex" ]] || { echo "ASSERT FAILED at -e:12" >&2; exit 1; }
side_packages=$("$HELPER" side-packages "$MANIFEST" | tr '\n' ' ' | sed 's/ $//')
[[ "$side_packages" == "ovm-codex-skew ovm-claudex" ]] || { echo "ASSERT FAILED at -e:14" >&2; exit 1; }
[[ $("$HELPER" main-package "$MANIFEST") == "ovm" ]] || { echo "ASSERT FAILED at -e:15" >&2; exit 1; }

cat > "$TMP_DIR/future.tsv" <<'EOF'
ovm-bundle-v1
main	ovm	ovm
side	ovm-codex-skew	ovm-codex-skew
side	ovm-claudex	ovm-claudex
side	ovm-future	ovm-future
EOF
"$HELPER" validate "$TMP_DIR/future.tsv"
[[ $("$HELPER" binaries "$TMP_DIR/future.tsv" | wc -l | tr -d ' ') == "4" ]] || { echo "ASSERT FAILED at -e:25" >&2; exit 1; }

cat > "$TMP_DIR/duplicate.tsv" <<'EOF'
ovm-bundle-v1
main	ovm	ovm
side	ovm-side	ovm-side
side	ovm-side	ovm-other
EOF
if "$HELPER" validate "$TMP_DIR/duplicate.tsv" 2>/dev/null; then
    echo "duplicate binary unexpectedly accepted" >&2
    exit 1
fi

cat > "$TMP_DIR/unsafe.tsv" <<'EOF'
ovm-bundle-v1
main	ovm	ovm
side	../ovm-side	ovm-side
EOF
if "$HELPER" validate "$TMP_DIR/unsafe.tsv" 2>/dev/null; then
    echo "unsafe binary unexpectedly accepted" >&2
    exit 1
fi

cat > "$TMP_DIR/no-main.tsv" <<'EOF'
ovm-bundle-v1
side	ovm-side	ovm-side
EOF
if "$HELPER" validate "$TMP_DIR/no-main.tsv" 2>/dev/null; then
    echo "manifest without main unexpectedly accepted" >&2
    exit 1
fi

echo "bundle-manifest: ok"
