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

PLATFORMS="darwin-arm64 darwin-x64 linux-x64 linux-arm64"
ARTIFACT_DIR="${OVM_NPM_ARTIFACT_DIR:-artifacts}"

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

# Every platform package must exist before anything is published. The loop
# below used to print "Skipping <platform> (no artifact)" and carry on — and
# the root package is still published with that platform in its
# optionalDependencies, so users on it get an install failure while the release
# reports success. A missing artifact is a broken release, not a skipped step.
require_all_artifacts() {
    missing=""
    for platform in $PLATFORMS; do
        target=$(target_for_platform "$platform")
        if [ ! -f "$ARTIFACT_DIR/ovm-${target}/ovm-${target}.tar.gz" ]; then
            missing="$missing $platform"
        fi
    done
    if [ -n "$missing" ]; then
        echo "ERROR: no build artifact for:$missing" >&2
        echo "       Publishing would ship a root package whose optionalDependencies" >&2
        echo "       name packages that do not exist. Refusing to publish." >&2
        return 1
    fi
    return 0
}

if [ -n "${OVM_NPM_PREFLIGHT_ONLY:-}" ]; then
    require_all_artifacts
    exit $?
fi
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

# pipefail-safe: /bin/sh with `set -e` only. `head -1` does cut grep off (its
# status is 141), but without pipefail the assignment takes cut's status, so the
# version still lands. Rewrite this line before adding `set -o pipefail`.
VERSION=$(cargo metadata --no-deps --format-version=1 | grep -o '"version":"[^"]*"' | head -1 | cut -d'"' -f4) # pipefail-safe: see the note above
NPM_TAG="${NPM_TAG:-latest}"
echo "Publishing OVM v${VERSION} to npm with dist-tag '${NPM_TAG}'..."

scripts/bundle-manifest.sh validate "$BUNDLE_MANIFEST"
require_all_artifacts

# Attach npm provenance in CI (requires the Actions OIDC token, so it is
# skipped for local runs).
publish_pkg() {
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
        npm publish --access public --tag "$NPM_TAG" --provenance
    else
        npm publish --access public --tag "$NPM_TAG"
    fi
}

# Resumable publishing: npm refuses to republish an existing version, so a
# rerun after a partial failure would die at the first already-published
# package and strand the rest. An exact-version hit on the registry is
# skipped instead. (`npm view` exits non-zero when the version is absent;
# a network failure also exits non-zero, which then fails loudly at the
# publish itself rather than silently skipping.)
already_published() {
    npm view "$1@${VERSION}" version >/dev/null 2>&1
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1 # pipefail-safe: cut consumes the single line sha256sum writes
    else
        shasum -a 256 "$1" | cut -d' ' -f1 # pipefail-safe: cut consumes the single line shasum writes
    fi
}

# Canonical metadata and content for every entry. Comparing only regular-file
# bytes would accept a package that changed an executable bit, symlink target,
# or entry type while retaining the same file payloads.
digest_tree() {
    node - "$1" <<'NODE'
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const root = process.argv[2];
const rows = [];

function walk(relative) {
  const directory = path.join(root, relative);
  for (const name of fs.readdirSync(directory).sort()) {
    const child = path.posix.join(relative, name);
    const fullPath = path.join(root, child);
    const stat = fs.lstatSync(fullPath);
    const mode = (stat.mode & 0o7777).toString(8).padStart(4, '0');
    if (stat.isSymbolicLink()) {
      rows.push(`link ${mode} ${child} -> ${fs.readlinkSync(fullPath)}`);
    } else if (stat.isDirectory()) {
      rows.push(`dir  ${mode} ${child}`);
      walk(child);
    } else if (stat.isFile()) {
      const digest = crypto.createHash('sha256').update(fs.readFileSync(fullPath)).digest('hex');
      rows.push(`file ${mode} ${digest} ${child}`);
    } else {
      rows.push(`other ${mode} ${child}`);
    }
  }
}

walk('');
process.stdout.write(`${rows.join('\n')}\n`);
NODE
}

# A matching version number is not a matching artifact. Skipping on the
# number alone means a rerun from a different tree reports success while the
# registry keeps serving the first upload's bytes — the release then claims
# to ship something nobody published. Prove the published tarball is the one
# this run would have produced, and fail loudly when it is not.
require_published_matches() {
    package=$1
    package_dir=$2
    compare_dir=$(mktemp -d "${TMPDIR:-/tmp}/ovm-npm-resume.XXXXXX")
    mkdir -p "$compare_dir/published" "$compare_dir/local"

    integrity=$(npm view "${package}@${VERSION}" dist.integrity 2>/dev/null || true)
    echo "    registry integrity: ${integrity:-unreported}"

    npm pack "${package}@${VERSION}" --pack-destination "$compare_dir/published" \
        >/dev/null 2>&1 || {
        echo "ERROR: could not download the published ${package}@${VERSION} tarball to compare against" >&2
        rm -rf "$compare_dir"
        return 1
    }
    (cd "$package_dir" && npm pack --pack-destination "$compare_dir/local" >/dev/null 2>&1) || {
        echo "ERROR: could not pack $package_dir to compare against the registry" >&2
        rm -rf "$compare_dir"
        return 1
    }

    published_tarball=$(find "$compare_dir/published" -name '*.tgz' -type f)
    local_tarball=$(find "$compare_dir/local" -name '*.tgz' -type f)
    if [ ! -f "$published_tarball" ] || [ ! -f "$local_tarball" ]; then
        echo "ERROR: expected exactly one packed tarball on each side for $package" >&2
        rm -rf "$compare_dir"
        return 1
    fi

    if [ "$(sha256_of "$published_tarball")" = "$(sha256_of "$local_tarball")" ]; then
        echo "    ${package}@${VERSION} on the registry is byte-identical to this build."
        rm -rf "$compare_dir"
        return 0
    fi

    # Tarball bytes can differ for reasons that are not content (packer
    # version, gzip settings), so fall through to the contents themselves
    # before calling it a mismatch.
    mkdir -p "$compare_dir/published-tree" "$compare_dir/local-tree"
    tar xzf "$published_tarball" -C "$compare_dir/published-tree"
    tar xzf "$local_tarball" -C "$compare_dir/local-tree"
    digest_tree "$compare_dir/published-tree" > "$compare_dir/published.sums"
    digest_tree "$compare_dir/local-tree" > "$compare_dir/local.sums"
    if ! diff -u "$compare_dir/published.sums" "$compare_dir/local.sums"; then
        echo "ERROR: ${package}@${VERSION} is already on the registry but its contents" >&2
        echo "       differ from what this run built. Refusing to report a publish that" >&2
        echo "       did not happen — publish a new version instead." >&2
        rm -rf "$compare_dir"
        return 1
    fi
    echo "    ${package}@${VERSION} on the registry has identical contents (tarball bytes differ)."
    rm -rf "$compare_dir"
    return 0
}

# Publish platform packages first
for platform in $PLATFORMS; do
    pkg_dir="npm/ovm-${platform}"
    target=$(target_for_platform "$platform")
    artifact="$ARTIFACT_DIR/ovm-${target}/ovm-${target}.tar.gz"

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

    if already_published "@mochiexists/ovm-${platform}"; then
        echo "  @mochiexists/ovm-${platform}@${VERSION} is already on the registry; verifying rather than skipping."
        require_published_matches "@mochiexists/ovm-${platform}" "$pkg_dir"
        continue
    fi
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

if already_published "@mochiexists/ovm"; then
    echo "  @mochiexists/ovm@${VERSION} is already on the registry; verifying rather than skipping."
    require_published_matches "@mochiexists/ovm" npm/ovm
else
    echo "  Publishing @mochiexists/ovm@${VERSION}..."
    cd npm/ovm && publish_pkg && cd -
fi

echo "Done."
