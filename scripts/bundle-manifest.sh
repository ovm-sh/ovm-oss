#!/bin/sh
# Validate and query OVM's public binary bundle manifest.
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
DEFAULT_MANIFEST="$SCRIPT_DIR/../crates/ovm/ovm-bundle-v1.tsv"
COMMAND=${1:-}
MANIFEST=${2:-$DEFAULT_MANIFEST}

usage() {
    echo "Usage: $0 <validate|binaries|entries|packages|side-packages|main-binary|main-package> [manifest]" >&2
    exit 2
}

validate_manifest() {
    awk -F '\t' '
        function fail(message) {
            print "ERROR: invalid bundle manifest: " message > "/dev/stderr"
            failed = 1
            exit 1
        }
        NR == 1 {
            if ($0 != "ovm-bundle-v1") {
                fail("unsupported or missing format header")
            }
            next
        }
        {
            if (NF != 3) {
                fail("line " NR " must contain exactly three tab-separated fields")
            }
            role = $1
            binary = $2
            package = $3

            if (role != "main" && role != "side") {
                fail("line " NR " has unknown role `" role "`")
            }
            if (binary !~ /^ovm(-[a-z0-9]+)*$/) {
                fail("line " NR " has unsafe binary name `" binary "`")
            }
            if (package != "-" && package !~ /^[a-z0-9]+(-[a-z0-9]+)*$/) {
                fail("line " NR " has unsafe Cargo package name `" package "`")
            }
            if (seen_binary[binary]++) {
                fail("duplicate binary `" binary "`")
            }
            if (package != "-" && seen_package[package]++) {
                fail("duplicate Cargo package `" package "`")
            }
            if (role == "main") {
                main_count++
                if (binary != "ovm" || package != "ovm") {
                    fail("the main row must be `main<TAB>ovm<TAB>ovm`")
                }
            }
            row_count++
        }
        END {
            if (failed) {
                exit 1
            }
            if (row_count == 0) {
                fail("manifest contains no binary rows")
            }
            if (main_count != 1) {
                fail("manifest must contain exactly one main row")
            }
        }
    ' "$MANIFEST"
}

[ -n "$COMMAND" ] || usage
[ -f "$MANIFEST" ] || {
    echo "ERROR: bundle manifest not found: $MANIFEST" >&2
    exit 1
}
validate_manifest

case "$COMMAND" in
    validate)
        ;;
    binaries)
        awk -F '\t' 'NR > 1 { print $2 }' "$MANIFEST"
        ;;
    entries)
        echo "ovm-bundle-v1.tsv"
        awk -F '\t' 'NR > 1 { print $2 }' "$MANIFEST"
        ;;
    packages)
        awk -F '\t' 'NR > 1 && $3 != "-" { print $3 }' "$MANIFEST"
        ;;
    side-packages)
        awk -F '\t' 'NR > 1 && $1 == "side" && $3 != "-" { print $3 }' "$MANIFEST"
        ;;
    main-binary)
        awk -F '\t' 'NR > 1 && $1 == "main" { print $2 }' "$MANIFEST"
        ;;
    main-package)
        awk -F '\t' 'NR > 1 && $1 == "main" { print $3 }' "$MANIFEST"
        ;;
    *)
        usage
        ;;
esac
