#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT
mkdir -p "$TMP_DIR/scripts" "$TMP_DIR/crates/ovm"
cp "$ROOT/scripts/bundle-manifest.sh" "$TMP_DIR/scripts/"
cp "$ROOT/scripts/update-brew-formula.sh" "$TMP_DIR/scripts/"

MANIFEST="$TMP_DIR/crates/ovm/ovm-bundle-v1.tsv"
cat > "$MANIFEST" <<'EOF'
ovm-bundle-v1
main	ovm	ovm
side	ovm-one	ovm-one
side	ovm-two	ovm-two
side	ovm-future	ovm-future
EOF

for target in \
  aarch64-apple-darwin \
  x86_64-apple-darwin \
  aarch64-unknown-linux-gnu \
  x86_64-unknown-linux-gnu
do
  artifact_dir="$TMP_DIR/artifacts/ovm-$target"
  bundle_dir="$TMP_DIR/bundle-$target"
  mkdir -p "$artifact_dir" "$bundle_dir"
  cp "$MANIFEST" "$bundle_dir/ovm-bundle-v1.tsv"
  for binary in ovm ovm-one ovm-two ovm-future; do
    printf '%s:%s\n' "$target" "$binary" > "$bundle_dir/$binary"
    chmod +x "$bundle_dir/$binary"
  done
  (cd "$bundle_dir" && tar czf "$artifact_dir/ovm-$target.tar.gz" \
    ovm-bundle-v1.tsv ovm ovm-one ovm-two ovm-future)
  LAST_ARCHIVE="$artifact_dir/ovm-$target.tar.gz"
done

OVM_NPM_VALIDATE_ARCHIVE="$LAST_ARCHIVE" \
OVM_BUNDLE_MANIFEST="$MANIFEST" \
sh "$ROOT/scripts/publish-npm.sh"
BAD_BUNDLE="$TMP_DIR/bad-bundle"
mkdir -p "$BAD_BUNDLE"
cp "$MANIFEST" "$BAD_BUNDLE/ovm-bundle-v1.tsv"
for binary in ovm ovm-one ovm-two ovm-future; do
  cp "$TMP_DIR/bundle-x86_64-unknown-linux-gnu/$binary" "$BAD_BUNDLE/$binary"
done
printf 'undeclared\n' > "$BAD_BUNDLE/ovm-undeclared"
(cd "$BAD_BUNDLE" && tar czf "$TMP_DIR/bad-bundle.tar.gz" \
  ovm-bundle-v1.tsv ovm ovm-one ovm-two ovm-future ovm-undeclared)
if OVM_NPM_VALIDATE_ARCHIVE="$TMP_DIR/bad-bundle.tar.gz" \
  OVM_BUNDLE_MANIFEST="$MANIFEST" \
  sh "$ROOT/scripts/publish-npm.sh"; then
  echo "npm archive validation accepted an undeclared entry" >&2
  exit 1
fi
rm "$BAD_BUNDLE/ovm-undeclared" "$BAD_BUNDLE/ovm-future"
ln -s ovm "$BAD_BUNDLE/ovm-future"
(cd "$BAD_BUNDLE" && tar czf "$TMP_DIR/symlink-bundle.tar.gz" \
  ovm-bundle-v1.tsv ovm ovm-one ovm-two ovm-future)
if OVM_NPM_VALIDATE_ARCHIVE="$TMP_DIR/symlink-bundle.tar.gz" \
  OVM_BUNDLE_MANIFEST="$MANIFEST" \
  sh "$ROOT/scripts/publish-npm.sh"; then
  echo "npm archive validation accepted a non-regular entry" >&2
  exit 1
fi

(
  cd "$TMP_DIR"
  GITHUB_REF_NAME=v0.0.1 sh scripts/update-brew-formula.sh >/dev/null
)
for binary in ovm ovm-one ovm-two ovm-future; do
  grep -Fq "bin.install \"$binary\"" "$TMP_DIR/Formula/ovm.rb"
done
[[ $(grep -c 'bin.install' "$TMP_DIR/Formula/ovm.rb") == 4 ]] || { echo "ASSERT FAILED at -e:75" >&2; exit 1; }
ruby -c "$TMP_DIR/Formula/ovm.rb" >/dev/null

platform=$(node -p 'process.platform + "-" + process.arch')
case "$platform" in
  darwin-arm64|darwin-x64|linux-arm64|linux-x64) ;;
  *) echo "unsupported Node test platform: $platform" >&2; exit 1 ;;
esac
npm_root="$TMP_DIR/npm-root"
platform_pkg="$npm_root/node_modules/@mochiexists/ovm-$platform"
mkdir -p "$npm_root" "$platform_pkg"
cp "$ROOT/npm/ovm/install.js" "$npm_root/install.js"
cp "$ROOT/npm/ovm-$platform/package.json" "$platform_pkg/package.json"
printf 'stale\n' > "$platform_pkg/ovm-obsolete"
printf 'stale manifest\n' > "$platform_pkg/ovm-bundle-v1.tsv"
OVM_NPM_CLEAN_DIR="$platform_pkg" sh "$ROOT/scripts/publish-npm.sh"
[[ ! -e "$platform_pkg/ovm-obsolete" ]] || { echo "ASSERT FAILED at -e:91" >&2; exit 1; }
[[ ! -e "$platform_pkg/ovm-bundle-v1.tsv" ]] || { echo "ASSERT FAILED at -e:92" >&2; exit 1; }
[[ -f "$platform_pkg/package.json" ]] || { echo "ASSERT FAILED at -e:93" >&2; exit 1; }
cp "$MANIFEST" "$platform_pkg/ovm-bundle-v1.tsv"
for binary in ovm ovm-one ovm-two ovm-future; do
  printf '%s\n' "$binary" > "$platform_pkg/$binary"
  chmod +x "$platform_pkg/$binary"
done
OVM_NPM_VALIDATE_DIR="$platform_pkg" \
OVM_BUNDLE_MANIFEST="$MANIFEST" \
sh "$ROOT/scripts/publish-npm.sh"
printf 'undeclared\n' > "$platform_pkg/ovm-undeclared"
if OVM_NPM_VALIDATE_DIR="$platform_pkg" \
  OVM_BUNDLE_MANIFEST="$MANIFEST" \
  sh "$ROOT/scripts/publish-npm.sh"; then
  echo "npm platform validation accepted an undeclared binary" >&2
  exit 1
fi
rm "$platform_pkg/ovm-undeclared"
PACK_JSON="$TMP_DIR/npm-pack.json"
(cd "$platform_pkg" && npm pack --dry-run --json > "$PACK_JSON")
node - "$PACK_JSON" <<'NODE'
const fs = require("fs");
const report = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
const names = new Set(report[0].files.map((file) => file.path));
for (const name of ["ovm", "ovm-one", "ovm-two", "ovm-future", "ovm-bundle-v1.tsv"]) {
  if (!names.has(name)) throw new Error(`npm pack omitted ${name}`);
}
if (names.has("ovm-obsolete")) throw new Error("npm pack retained an obsolete side binary");
NODE
node "$npm_root/install.js"
for binary in ovm ovm-one ovm-two ovm-future; do
  [[ -x "$npm_root/bin/$binary" ]] || { echo "ASSERT FAILED at -e:123" >&2; exit 1; }
done
cmp "$MANIFEST" "$npm_root/bin/ovm-bundle-v1.tsv"

echo "bundle-contract: ok"
