#!/bin/sh
# Publish OVM npm packages after CI builds the platform binaries.
# Called by .github/workflows/release.yml
set -e

BUNDLE_MANIFEST="${OVM_BUNDLE_MANIFEST:-crates/ovm/ovm-bundle-v1.tsv}"

# sed -i.bak leaves stale backups behind if the run is interrupted between
# the edit and its rm; sweep them on any exit so they can't be committed or
# picked up by a later publish. Signal traps must exit explicitly — a bare
# handler would swallow the signal and let the publish loop keep going.
trap 'rm -f npm/*/package.json.bak' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

clean_platform_bundle() {
    package_dir=$1
    rm -f "$package_dir/ovm" "$package_dir"/ovm-*
}

validate_archive_entries() {
    archive=$1
    manifest=$2
    expected=$(mktemp "${TMPDIR:-/tmp}/ovm-archive-expected.XXXXXX")
    actual=$(mktemp "${TMPDIR:-/tmp}/ovm-archive-actual.XXXXXX")
    verbose=$(mktemp "${TMPDIR:-/tmp}/ovm-archive-verbose.XXXXXX")
    scripts/bundle-manifest.sh entries "$manifest" | sort > "$expected"
    tar tzf "$archive" | sort > "$actual"
    tar tvzf "$archive" > "$verbose"
    if cmp -s "$expected" "$actual" && awk '$1 !~ /^-/ { exit 1 }' "$verbose"; then
        rm -f "$expected" "$actual" "$verbose"
        return 0
    fi
    rm -f "$expected" "$actual" "$verbose"
    return 1
}

validate_platform_bundle() {
    package_dir=$1
    manifest=$2
    expected=$(mktemp "${TMPDIR:-/tmp}/ovm-npm-expected.XXXXXX")
    actual=$(mktemp "${TMPDIR:-/tmp}/ovm-npm-actual.XXXXXX")
    {
        echo "ovm-bundle-v1.tsv"
        scripts/bundle-manifest.sh binaries "$manifest"
    } | sort > "$expected"
    for candidate in "$package_dir/ovm" "$package_dir"/ovm-*; do
        if [ -f "$candidate" ]; then
            basename "$candidate"
        fi
    done | sort > "$actual"
    if cmp -s "$expected" "$actual"; then
        rm -f "$expected" "$actual"
        return 0
    fi
    rm -f "$expected" "$actual"
    return 1
}

if [ -n "${OVM_NPM_CLEAN_DIR:-}" ]; then
    clean_platform_bundle "$OVM_NPM_CLEAN_DIR"
    exit 0
fi
if [ -n "${OVM_NPM_VALIDATE_DIR:-}" ]; then
    scripts/bundle-manifest.sh validate "$BUNDLE_MANIFEST"
    validate_platform_bundle "$OVM_NPM_VALIDATE_DIR" "$BUNDLE_MANIFEST"
    exit $?
fi
if [ -n "${OVM_NPM_VALIDATE_ARCHIVE:-}" ]; then
    scripts/bundle-manifest.sh validate "$BUNDLE_MANIFEST"
    validate_archive_entries "$OVM_NPM_VALIDATE_ARCHIVE" "$BUNDLE_MANIFEST"
    exit $?
fi

VERSION=$(cargo metadata --no-deps --format-version=1 | grep -o '"version":"[^"]*"' | head -1 | cut -d'"' -f4)
NPM_TAG="${NPM_TAG:-latest}"
echo "Publishing OVM v${VERSION} to npm with dist-tag '${NPM_TAG}'..."

PLATFORMS="darwin-arm64 darwin-x64 linux-x64 linux-arm64"
scripts/bundle-manifest.sh validate "$BUNDLE_MANIFEST"

# Attach npm provenance in CI (requires the Actions OIDC token, so it is
# skipped for local runs).
publish_pkg() {
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
        npm publish --access public --tag "$NPM_TAG" --provenance
    else
        npm publish --access public --tag "$NPM_TAG"
    fi
}

target_for_platform() {
    case "$1" in
        darwin-arm64) echo "aarch64-apple-darwin" ;;
        darwin-x64) echo "x86_64-apple-darwin" ;;
        linux-x64) echo "x86_64-unknown-linux-gnu" ;;
        linux-arm64) echo "aarch64-unknown-linux-gnu" ;;
        *)
            echo "Unknown platform: $1" >&2
            exit 1
            ;;
    esac
}

# Publish platform packages first
for platform in $PLATFORMS; do
    pkg_dir="npm/ovm-${platform}"
    target=$(target_for_platform "$platform")
    artifact="artifacts/ovm-${target}/ovm-${target}.tar.gz"

    if [ ! -f "$artifact" ]; then
        echo "  Skipping $platform (no artifact at $artifact)"
        continue
    fi

    if ! validate_archive_entries "$artifact" "$BUNDLE_MANIFEST"; then
        echo "ERROR: $artifact contents differ from its bundle manifest" >&2
        exit 1
    fi

    # Remove the previous generated bundle before extracting. The package files
    # glob includes ovm-*, so a side binary removed from the new manifest must not linger.
    clean_platform_bundle "$pkg_dir"
    tar xzf "$artifact" -C "$pkg_dir"
    scripts/bundle-manifest.sh validate "$pkg_dir/ovm-bundle-v1.tsv"
    if ! cmp -s "$BUNDLE_MANIFEST" "$pkg_dir/ovm-bundle-v1.tsv"; then
        echo "ERROR: $artifact bundle manifest differs from the release source" >&2
        exit 1
    fi
    if ! validate_platform_bundle "$pkg_dir" "$BUNDLE_MANIFEST"; then
        echo "ERROR: $artifact contents differ from its bundle manifest" >&2
        exit 1
    fi

    # Stamp version
    sed -i.bak "s/\"version\": \"0.0.0\"/\"version\": \"${VERSION}\"/" "$pkg_dir/package.json"
    rm -f "$pkg_dir/package.json.bak"

    echo "  Publishing @mochiexists/ovm-${platform}@${VERSION}..."
    cd "$pkg_dir" && publish_pkg && cd -
done

# Publish root package
sed -i.bak "s/\"version\": \"0.0.0\"/\"version\": \"${VERSION}\"/" npm/ovm/package.json
rm -f npm/ovm/package.json.bak

# Update optional dependency versions
for platform in $PLATFORMS; do
    sed -i.bak "s/\"@mochiexists\/ovm-${platform}\": \"0.0.0\"/\"@mochiexists\/ovm-${platform}\": \"${VERSION}\"/" npm/ovm/package.json
    rm -f npm/ovm/package.json.bak
done

echo "  Publishing @mochiexists/ovm@${VERSION}..."
cd npm/ovm && publish_pkg && cd -

echo "Done."
