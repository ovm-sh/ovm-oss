#!/bin/sh
# Generate Formula/ovm.rb from the just-built release artifacts.
# Called by .github/workflows/release.yml in the publish-brew job.
#
# Inputs:
#   - $GITHUB_REF_NAME (e.g. "v0.0.1") OR falls back to crates/ovm/Cargo.toml
#   - $OVM_BREW_FORMULA_NAME (optional: "ovm" or "ovm-beta"; default "ovm")
#   - artifacts/ovm-<target>/ovm-<target>.tar.gz for each of the 4 supported targets
#
# Output:
#   - Formula/<formula>.rb (overwrites any existing file)
set -e

VERSION="${GITHUB_REF_NAME#v}"
if [ -z "$VERSION" ] || [ "$VERSION" = "$GITHUB_REF_NAME" ]; then
    VERSION=$(grep -m1 '^version = ' crates/ovm/Cargo.toml | sed -E 's/version = "(.*)"/\1/')
fi
FORMULA_NAME="${OVM_BREW_FORMULA_NAME:-ovm}"
BUNDLE_MANIFEST="crates/ovm/ovm-bundle-v1.tsv"
scripts/bundle-manifest.sh validate "$BUNDLE_MANIFEST"
case "$FORMULA_NAME" in
    ovm)
        FORMULA_CLASS="Ovm"
        ;;
    ovm-beta)
        FORMULA_CLASS="OvmBeta"
        ;;
    *)
        echo "ERROR: unsupported formula name '$FORMULA_NAME'" >&2
        exit 1
        ;;
esac

DARWIN_ARM_TGZ="artifacts/ovm-aarch64-apple-darwin/ovm-aarch64-apple-darwin.tar.gz"
DARWIN_X64_TGZ="artifacts/ovm-x86_64-apple-darwin/ovm-x86_64-apple-darwin.tar.gz"
LINUX_ARM_TGZ="artifacts/ovm-aarch64-unknown-linux-gnu/ovm-aarch64-unknown-linux-gnu.tar.gz"
LINUX_X64_TGZ="artifacts/ovm-x86_64-unknown-linux-gnu/ovm-x86_64-unknown-linux-gnu.tar.gz"

TMP_DIR=$(mktemp -d 2>/dev/null || mktemp -d -t ovm-brew)
cleanup() {
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT INT TERM

for f in "$DARWIN_ARM_TGZ" "$DARWIN_X64_TGZ" "$LINUX_ARM_TGZ" "$LINUX_X64_TGZ"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing artifact $f" >&2
        exit 1
    fi
    archive_manifest="$TMP_DIR/$(basename "$f").manifest"
    if ! tar xOf "$f" ovm-bundle-v1.tsv > "$archive_manifest"; then
        echo "ERROR: $f has no ovm-bundle-v1.tsv" >&2
        exit 1
    fi
    scripts/bundle-manifest.sh validate "$archive_manifest"
    if ! cmp -s "$BUNDLE_MANIFEST" "$archive_manifest"; then
        echo "ERROR: $f bundle manifest differs from $BUNDLE_MANIFEST" >&2
        exit 1
    fi
done

sha() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

SHA_DARWIN_ARM=$(sha "$DARWIN_ARM_TGZ")
SHA_DARWIN_X64=$(sha "$DARWIN_X64_TGZ")
SHA_LINUX_ARM=$(sha "$LINUX_ARM_TGZ")
SHA_LINUX_X64=$(sha "$LINUX_X64_TGZ")

BASE="https://github.com/ovm-sh/ovm/releases/download/v${VERSION}"

mkdir -p Formula
{
    cat <<EOF
class ${FORMULA_CLASS} < Formula
  desc "Open Version Manager for AI coding tools"
  homepage "https://github.com/ovm-sh/ovm"
  version "${VERSION}"
  license "MIT"

  on_macos do
    on_arm do
      url "${BASE}/ovm-aarch64-apple-darwin.tar.gz"
      sha256 "${SHA_DARWIN_ARM}"
    end
    on_intel do
      url "${BASE}/ovm-x86_64-apple-darwin.tar.gz"
      sha256 "${SHA_DARWIN_X64}"
    end
  end

  on_linux do
    on_arm do
      url "${BASE}/ovm-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "${SHA_LINUX_ARM}"
    end
    on_intel do
      url "${BASE}/ovm-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "${SHA_LINUX_X64}"
    end
  end

  def install
EOF
    while IFS= read -r binary; do
        printf '    bin.install "%s"\n' "$binary"
    done <<EOF
$(scripts/bundle-manifest.sh binaries "$BUNDLE_MANIFEST")
EOF
    cat <<'EOF'
  end

  test do
    system "#{bin}/ovm", "--version"
  end
end
EOF
} > "Formula/${FORMULA_NAME}.rb"

echo "Wrote Formula/${FORMULA_NAME}.rb for v${VERSION}"
