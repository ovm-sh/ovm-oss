#!/bin/sh
# OVM direct installer — installs a verified, self-managed binary bundle.
# Usage: curl -fsSL https://raw.githubusercontent.com/ovm-sh/ovm-oss/main/install.sh | sh
set -eu
# Fail a pipeline if any stage fails, not just the last — so a broken `curl` in
# a `curl | grep | cut` can't be masked by a succeeding downstream command.
# pipefail is not POSIX (dash lacks it), so enable it only where supported.
# shellcheck disable=SC3040  # guarded: the subshell probe skips it on POSIX sh
if (set -o pipefail) 2>/dev/null; then set -o pipefail; fi

REPO="ovm-sh/ovm-oss"
BINARY="ovm"
MANIFEST_NAME="ovm-bundle-v1.tsv"
INSTALL_DIR="${OVM_INSTALL_DIR:-$HOME/.ovm/bin}"
SELF_ROOT="$HOME/.ovm/self"
VERSIONS_DIR="$SELF_ROOT/versions"
CURRENT_LINK="$SELF_ROOT/current"
PREVIOUS_LINK="$SELF_ROOT/previous"
LOCAL_ARTIFACT_DIR="${OVM_LOCAL_ARTIFACT_DIR:-}"
LOCAL_MANIFEST="${OVM_LOCAL_MANIFEST:-}"
LOCAL_VERSION="${OVM_LOCAL_VERSION:-}"
LEGACY_ROOT="${OVM_LEGACY_ROOT:-}"
# A trailing slash would silently defeat the "$LEGACY_ROOT"/* migration match.
while [ "${LEGACY_ROOT%/}" != "$LEGACY_ROOT" ]; do LEGACY_ROOT="${LEGACY_ROOT%/}"; done
STAGING_DIR=""
STATE_BACKUP=""
TMP_DIR=""
OPERATION_LOCK="$SELF_ROOT/.operation.lock"
LOCK_FIFO=""
LOCK_READY=""
LOCK_HELPER_PID=""
LOCK_PIPE_OPEN=0
ROLLBACK_ON_CLEANUP=0

cleanup() {
    cleanup_status=$?
    trap - EXIT INT TERM
    if [ "$ROLLBACK_ON_CLEANUP" = "1" ] \
        && [ -n "$STATE_BACKUP" ] \
        && [ -d "$STATE_BACKUP" ]; then
        ROLLBACK_ON_CLEANUP=0
        if restore_install_state; then
            STATE_RESTORE_OK=1
        else
            STATE_RESTORE_OK=0
        fi
    fi
    if [ -n "$STAGING_DIR" ] && [ -d "$STAGING_DIR" ]; then
        rm -rf "${STAGING_DIR:?}"
    fi
    if [ -n "$STATE_BACKUP" ] && [ -d "$STATE_BACKUP" ]; then
        # Only discard the recovery snapshot when we did NOT run an incomplete
        # rollback. A partial restore keeps its backup so the user can recover
        # by hand rather than being left half-installed with nothing to fall
        # back to.
        if [ "${STATE_RESTORE_OK:-1}" = "1" ]; then
            rm -rf "${STATE_BACKUP:?}"
        else
            echo "Warning: rollback did not fully complete; preserving recovery snapshot at $STATE_BACKUP" >&2
        fi
    fi
    if [ -n "$TMP_DIR" ] && [ -d "$TMP_DIR" ]; then
        rm -rf "${TMP_DIR:?}"
    fi
    if [ "$LOCK_PIPE_OPEN" = "1" ]; then
        exec 9>&-
        LOCK_PIPE_OPEN=0
    fi
    if [ -n "$LOCK_HELPER_PID" ]; then
        kill "$LOCK_HELPER_PID" 2>/dev/null || true
        wait "$LOCK_HELPER_PID" 2>/dev/null || true
        LOCK_HELPER_PID=""
    fi
    [ -n "$LOCK_FIFO" ] && rm -f "$LOCK_FIFO"
    [ -n "$LOCK_READY" ] && rm -f "$LOCK_READY"
        exit "$cleanup_status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

fail() {
    echo "Error: $*" >&2
    exit 1
}

acquire_operation_lock() {
    helper=$1
    mkdir -p "$SELF_ROOT"
    LOCK_FIFO="$SELF_ROOT/.operation-lock-fifo.$$"
    LOCK_READY="$SELF_ROOT/.operation-lock-ready.$$"
    rm -f "$LOCK_FIFO" "$LOCK_READY"
    mkfifo "$LOCK_FIFO"
    OVM_SELF_LOCK_HELPER_PATH="$OPERATION_LOCK" \
    OVM_SELF_LOCK_HELPER_READY="$LOCK_READY" \
    "$helper" < "$LOCK_FIFO" &
    LOCK_HELPER_PID=$!
    exec 9> "$LOCK_FIFO"
    LOCK_PIPE_OPEN=1

    attempts=0
    announced=0
    while [ ! -f "$LOCK_READY" ]; do
        if ! kill -0 "$LOCK_HELPER_PID" 2>/dev/null; then
            fail "OVM self-management lock helper exited before acquiring the lock"
        fi
        attempts=$((attempts + 1))
        if [ "$attempts" -ge 1200 ]; then
            fail "timed out waiting for another OVM self-management operation"
        fi
        if [ "$announced" = "0" ]; then
            echo "Waiting for another OVM self-management operation to finish..." >&2
            announced=1
        fi
        sleep 0.05
    done
}

validate_manifest() {
    _ovm_manifest_path=$1
    awk -F '\t' '
        function fail_manifest(message) {
            print "Error: invalid bundle manifest: " message > "/dev/stderr"
            failed = 1
            exit 1
        }
        NR == 1 {
            if ($0 != "ovm-bundle-v1") {
                fail_manifest("unsupported or missing format header")
            }
            next
        }
        {
            if (NF != 3) {
                fail_manifest("line " NR " must contain exactly three tab-separated fields")
            }
            role = $1
            binary = $2
            package = $3
            if (role != "main" && role != "side") {
                fail_manifest("line " NR " has unknown role `" role "`")
            }
            if (binary !~ /^ovm(-[a-z0-9]+)*$/) {
                fail_manifest("line " NR " has unsafe binary name `" binary "`")
            }
            if (package != "-" && package !~ /^[a-z0-9]+(-[a-z0-9]+)*$/) {
                fail_manifest("line " NR " has unsafe Cargo package `" package "`")
            }
            if (seen_binary[binary]++) {
                fail_manifest("duplicate binary `" binary "`")
            }
            if (package != "-" && seen_package[package]++) {
                fail_manifest("duplicate Cargo package `" package "`")
            }
            if (role == "main") {
                main_count++
                if (binary != "ovm" || package != "ovm") {
                    fail_manifest("the main row must be main<TAB>ovm<TAB>ovm")
                }
            }
            rows++
        }
        END {
            if (failed) {
                exit 1
            }
            if (rows == 0) {
                fail_manifest("manifest contains no binaries")
            }
            if (main_count != 1) {
                fail_manifest("manifest must contain exactly one main row")
            }
        }
    ' "$_ovm_manifest_path"
}

manifest_binaries() {
    awk -F '\t' 'NR > 1 { print $2 }' "$1"
}

manifest_side_binaries() {
    awk -F '\t' 'NR > 1 && $1 == "side" { print $2 }' "$1"
}

managed_side_names() {
    awk '
        NF == 0 { next }
        $0 == "ovm" || $0 !~ /^ovm(-[a-z0-9]+)*$/ || seen[$0]++ { exit 1 }
        { print }
    ' "$1"
}

validate_version() {
    case "$1" in
        ""|.*|*[!A-Za-z0-9._+-]*) fail "invalid self version identifier '$1'" ;;
    esac
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        fail "sha256sum or shasum is required"
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

validate_link_path() {
    link=$1
    if [ -e "$link" ] && [ ! -L "$link" ]; then
        fail "refusing to replace non-symlink pointer at $link"
    fi
}

switch_link() {
    link=$1
    target=$2
    validate_link_path "$link"
    parent=$(dirname "$link")
    temp="$parent/.ovm-link-$$"
    rm -f "$temp"
    ln -s "$target" "$temp"
    replace_path "$temp" "$link"
}

atomic_copy() {
    source=$1
    destination=$2
    parent=$(dirname "$destination")
    temp=$(mktemp "$parent/.ovm-copy.XXXXXX")
    cp "$source" "$temp"
    chmod 755 "$temp"
    replace_path "$temp" "$destination"
}

snapshot_path() {
    key=$1
    path=$2
    printf '%s\n' "$path" > "$STATE_BACKUP/$key.path"
    if [ -L "$path" ]; then
        printf 'symlink\n' > "$STATE_BACKUP/$key.type"
        readlink "$path" > "$STATE_BACKUP/$key.target"
    elif [ -f "$path" ]; then
        printf 'file\n' > "$STATE_BACKUP/$key.type"
        cp -p "$path" "$STATE_BACKUP/$key.file"
    elif [ -e "$path" ]; then
        fail "refusing to snapshot unsupported path at $path"
    else
        printf 'missing\n' > "$STATE_BACKUP/$key.type"
    fi
}

restore_path() {
    # Runs only during rollback. Every failure path must `return 1` (not
    # `fail`/exit) so restore_install_state can aggregate it and cleanup can
    # preserve the recovery snapshot; a hard exit here would bypass that and
    # drop the backup.
    key=$1
    path=$(sed -n '1p' "$STATE_BACKUP/$key.path")
    path_type=$(sed -n '1p' "$STATE_BACKUP/$key.type")
    case "$path_type" in
        missing)
            if [ -d "$path" ] && [ ! -L "$path" ]; then
                echo "Error: refusing to replace directory while restoring $path" >&2
                return 1
            fi
            rm -f "$path" || return 1
            ;;
        symlink)
            if [ -d "$path" ] && [ ! -L "$path" ]; then
                echo "Error: refusing to replace directory while restoring $path" >&2
                return 1
            fi
            rm -f "$path" || return 1
            switch_link "$path" "$(sed -n '1p' "$STATE_BACKUP/$key.target")" || return 1
            ;;
        file)
            if [ -d "$path" ] && [ ! -L "$path" ]; then
                echo "Error: refusing to replace directory while restoring $path" >&2
                return 1
            fi
            parent=$(dirname "$path")
            temp=$(mktemp "$parent/.ovm-restore.XXXXXX") || return 1
            cp -p "$STATE_BACKUP/$key.file" "$temp" || return 1
            replace_path "$temp" "$path" || return 1
            ;;
        *)
            echo "Error: invalid OVM state snapshot for $path" >&2
            return 1
            ;;
    esac
}

restore_install_state() {
    # Recover the control plane and the active selection FIRST so an
    # interrupted rollback still leaves a working `ovm` pointing at the
    # previous version rather than a launcher pointing at the half-installed
    # one. Attempt EVERY path and remember whether any failed instead of
    # aborting on the first — a partial restore is worse than a best-effort
    # full one — and report the aggregate so the caller can keep the backup.
    restore_rc=0
    for restore_target in control current control-previous previous launcher-dir side-links; do
        restore_path "$restore_target" || restore_rc=1
    done
    while IFS= read -r restore_key; do
        [ -n "$restore_key" ] || continue
        restore_path "$restore_key" || restore_rc=1
    done < "$STATE_BACKUP/launcher-keys"
    return "$restore_rc"
}

bundle_matches() {
    installed=$1
    manifest=$2
    source_dir=$3
    [ -f "$installed/.complete" ] || return 1
    cmp -s "$manifest" "$installed/$MANIFEST_NAME" || return 1
    while IFS= read -r binary; do
        [ -f "$installed/$binary" ] || return 1
        cmp -s "$source_dir/$binary" "$installed/$binary" || return 1
    done <<EOF
$(manifest_binaries "$manifest")
EOF
}

legacy_direct_side_path() {
    _ovm_legacy_path=$1
    [ -f "$_ovm_legacy_path" ] || return 1
    [ ! -L "$_ovm_legacy_path" ] || return 1
    case "$(basename "$_ovm_legacy_path")" in
        ovm-codex-skew|ovm-claudex) return 0 ;;
        *) return 1 ;;
    esac
}

# A control plane left behind by the retired checkout-symlink developer
# workflow points into the checkout ($LEGACY_ROOT). It is ours to replace, not
# a foreign install: the fresh control plane is written in its place and the
# original symlink is snapshotted for rollback like any other launcher.
legacy_checkout_link() {
    _lcl_link=$1
    [ -L "$_lcl_link" ] || return 1
    [ -n "$LEGACY_ROOT" ] || return 1
    case "$(readlink "$_lcl_link")" in
        "$LEGACY_ROOT"/*) return 0 ;;
        *) return 1 ;;
    esac
}

managed_side_link() {
    link=$1
    [ -L "$link" ] || return 1
    target=$(readlink "$link")
    case "$target" in
        ovm|"$INSTALL_DIR_ABS/ovm") return 0 ;;
    esac
    if [ -n "$LEGACY_ROOT" ]; then
        case "$target" in
            "$LEGACY_ROOT"/*) return 0 ;;
        esac
    fi

    case "$target" in
        /*) absolute=$target ;;
        *) absolute=$(dirname "$link")/$target ;;
    esac
    resolved_parent=$(CDPATH='' cd "$(dirname "$absolute")" 2>/dev/null && pwd -P) || return 1
    resolved="$resolved_parent/$(basename "$absolute")"
    case "$resolved" in
        "$VERSIONS_DIR_REAL"/*) return 0 ;;
    esac
    return 1
}

validate_archive() {
    archive=$1
    extract_dir=$2
    verbose="$TMP_DIR/archive.verbose"
    entries="$TMP_DIR/archive.entries"
    expected="$TMP_DIR/archive.expected"

    tar tvzf "$archive" > "$verbose"
    if awk '$1 !~ /^-/ { exit 1 }' "$verbose"; then
        :
    else
        fail "release archive contains a non-regular entry"
    fi

    tar tzf "$archive" > "$entries"
    if grep -qvE '^(ovm-bundle-v1\.tsv|ovm(-[a-z0-9]+)*)$' "$entries"; then
        fail "release archive contains an unsafe or unexpected path"
    fi
    [ "$(grep -c "^$MANIFEST_NAME$" "$entries")" -eq 1 ] ||
        fail "release archive must contain exactly one $MANIFEST_NAME"

    mkdir -p "$extract_dir"
    tar xzf "$archive" -C "$extract_dir"
    validate_manifest "$extract_dir/$MANIFEST_NAME"

    {
        echo "$MANIFEST_NAME"
        manifest_binaries "$extract_dir/$MANIFEST_NAME"
    } | sort > "$expected"
    sort "$entries" > "$entries.sorted"
    if ! cmp -s "$expected" "$entries.sorted"; then
        fail "release archive contents do not match its bundle manifest"
    fi
}

install_bundle() {
    version=$1
    manifest=$2
    source_dir=$3
    validate_version "$version"
    validate_manifest "$manifest"

    while IFS= read -r binary; do
        [ -f "$source_dir/$binary" ] || fail "bundle is missing $binary"
    done <<EOF
$(manifest_binaries "$manifest")
EOF

    lock_helper=${OVM_SELF_LOCK_HELPER_BINARY:-$source_dir/ovm}
    acquire_operation_lock "$lock_helper"
    mkdir -p "$INSTALL_DIR" "$VERSIONS_DIR"
    INSTALL_DIR_ABS=$(CDPATH='' cd "$INSTALL_DIR" && pwd -P)
    VERSIONS_DIR_REAL=$(CDPATH='' cd "$VERSIONS_DIR" && pwd -P)
    control="$INSTALL_DIR_ABS/ovm"
    owned_control=0
    legacy_direct=0
    if [ -e "$control" ] || [ -L "$control" ]; then
        if [ -f "$SELF_ROOT/launcher-dir" ] \
            && [ "$(sed -n '1p' "$SELF_ROOT/launcher-dir")" = "$INSTALL_DIR_ABS" ] \
            && [ -f "$control" ] \
            && [ ! -L "$control" ]; then
            owned_control=1
        elif [ -f "$control" ] \
            && [ ! -L "$control" ] \
            && [ ! -e "$SELF_ROOT/launcher-dir" ] \
            && [ "${OVM_MIGRATE_LEGACY_DIRECT:-0}" = "1" ]; then
            owned_control=1
            legacy_direct=1
        elif legacy_checkout_link "$control"; then
            # Retired checkout-symlink workflow. Leave owned_control=0 so the
            # fresh control plane is written (refresh_control=1) over the
            # symlink and the original is snapshotted for rollback.
            echo "  Migrating legacy checkout control plane: $control"
        else
            fail "refusing to replace foreign OVM control plane at $control"
        fi
    fi
    final_dir="$VERSIONS_DIR/$version"

    if [ -e "$final_dir" ]; then
        if ! bundle_matches "$final_dir" "$manifest" "$source_dir"; then
            fail "self version $version already exists with different contents"
        fi
    else
        STAGING_DIR=$(mktemp -d "$VERSIONS_DIR/.installing.XXXXXX")
        cp "$manifest" "$STAGING_DIR/$MANIFEST_NAME"
        while IFS= read -r binary; do
            cp "$source_dir/$binary" "$STAGING_DIR/$binary"
            chmod 755 "$STAGING_DIR/$binary"
        done <<EOF
$(manifest_binaries "$manifest")
EOF
        : > "$STAGING_DIR/.complete"
        if [ -e "$final_dir" ]; then
            if bundle_matches "$final_dir" "$manifest" "$source_dir"; then
                rm -rf "${STAGING_DIR:?}"
                STAGING_DIR=""
            else
                fail "self version $version appeared with different contents during installation"
            fi
        else
            mv "$STAGING_DIR" "$final_dir"
            STAGING_DIR=""
        fi
    fi

    old_manifest=""
    old_target=""
    if [ -L "$CURRENT_LINK" ]; then
        old_target=$(readlink "$CURRENT_LINK")
        case "$old_target" in
            /*) old_dir=$old_target ;;
            *) old_dir="$SELF_ROOT/$old_target" ;;
        esac
        if [ -f "$old_dir/$MANIFEST_NAME" ]; then
            if validate_manifest "$old_dir/$MANIFEST_NAME" >/dev/null 2>&1; then
                old_manifest="$old_dir/$MANIFEST_NAME"
            else
                echo "Warning: active bundle manifest is corrupt; preserving its side links" >&2
            fi
        fi
    fi
    old_side_names=""
    if [ -f "$SELF_ROOT/side-links" ]; then
        if old_side_names=$(managed_side_names "$SELF_ROOT/side-links"); then
            :
        else
            old_side_names=""
            echo "Warning: ignoring corrupt managed side-link record" >&2
            if [ -n "$old_manifest" ]; then
                old_side_names=$(manifest_side_binaries "$old_manifest")
            fi
        fi
    elif [ -n "$old_manifest" ]; then
        old_side_names=$(manifest_side_binaries "$old_manifest")
    fi

    # Validate every pointer and side path before changing any live entry.
    validate_link_path "$CURRENT_LINK"
    if [ -n "$old_target" ] && [ "$old_target" != "$final_dir" ]; then
        validate_link_path "$PREVIOUS_LINK"
    fi
    while IFS= read -r binary; do
        [ -n "$binary" ] || continue
        side_link="$INSTALL_DIR_ABS/$binary"
        if [ -e "$side_link" ] || [ -L "$side_link" ]; then
            if [ "$legacy_direct" = "1" ] && legacy_direct_side_path "$side_link"; then
                :
            else
                managed_side_link "$side_link" ||
                    fail "refusing to replace foreign side binary at $side_link"
            fi
        fi
    done <<EOF
$(manifest_side_binaries "$manifest")
EOF

    STATE_BACKUP=$(mktemp -d "${TMPDIR:-/tmp}/ovm-state.XXXXXX")
    snapshot_path current "$CURRENT_LINK"
    snapshot_path previous "$PREVIOUS_LINK"
    snapshot_path launcher-dir "$SELF_ROOT/launcher-dir"
    snapshot_path side-links "$SELF_ROOT/side-links"
    snapshot_path control "$control"
    snapshot_path control-previous "$SELF_ROOT/control-previous"
    : > "$STATE_BACKUP/launcher-keys"
    {
        printf '%s\n' "$old_side_names"
        manifest_side_binaries "$manifest"
    } | awk 'NF && !seen[$0]++' > "$STATE_BACKUP/side-names"
    snapshot_index=0
    while IFS= read -r binary; do
        [ -n "$binary" ] || continue
        path="$INSTALL_DIR_ABS/$binary"
        if [ -e "$path" ] && [ ! -L "$path" ]; then
            if [ "$legacy_direct" != "1" ] || ! legacy_direct_side_path "$path"; then
                continue
            fi
        fi
        if [ -L "$path" ] && ! managed_side_link "$path"; then
            continue
        fi
        key="side-$snapshot_index"
        snapshot_path "$key" "$path"
        printf '%s\n' "$key" >> "$STATE_BACKUP/launcher-keys"
        snapshot_index=$((snapshot_index + 1))
    done < "$STATE_BACKUP/side-names"
    for product in claude codex pi; do
        path="$HOME/.ovm/bin/$product"
        if [ -L "$path" ] && managed_side_link "$path"; then
            key="product-$product"
            snapshot_path "$key" "$path"
            printf '%s\n' "$key" >> "$STATE_BACKUP/launcher-keys"
        fi
    done
    ROLLBACK_ON_CLEANUP=1

    set +e
    (
        trap - EXIT INT TERM
        set -e
        launcher_temp=$(mktemp "$SELF_ROOT/.launcher-dir.XXXXXX")
        printf '%s\n' "$INSTALL_DIR_ABS" > "$launcher_temp"
    replace_path "$launcher_temp" "$SELF_ROOT/launcher-dir"

    refresh_control=${OVM_REFRESH_CONTROL:-}
    if [ -z "$refresh_control" ]; then
        if [ -n "$LOCAL_ARTIFACT_DIR" ] \
            && [ "$owned_control" = "1" ] \
            && [ "$legacy_direct" = "0" ]; then
            refresh_control=0
        else
            refresh_control=1
        fi
    fi
    if [ "$refresh_control" = "1" ]; then
        if [ -f "$control" ]; then
            previous_control=$(mktemp "$SELF_ROOT/.control-previous.XXXXXX")
            cp "$control" "$previous_control"
            chmod 755 "$previous_control"
            replace_path "$previous_control" "$SELF_ROOT/control-previous"
        fi
        atomic_copy "$final_dir/ovm" "$control"
    else
        echo "  Preserved control plane: $control"
    fi

    while IFS= read -r binary; do
        [ -n "$binary" ] || continue
        side_link="$INSTALL_DIR_ABS/$binary"
        if [ "$legacy_direct" = "1" ] && legacy_direct_side_path "$side_link"; then
            rm -f "$side_link"
        fi
        switch_link "$side_link" ovm
    done <<EOF
$(manifest_side_binaries "$manifest")
EOF

    if [ -n "$old_target" ] && [ "$old_target" != "$final_dir" ]; then
        switch_link "$PREVIOUS_LINK" "$old_target"
    fi
    switch_link "$CURRENT_LINK" "$final_dir"
    if [ "${OVM_TEST_FAIL_AFTER_CURRENT:-0}" = "1" ]; then
        echo "Error: injected activation failure after switching current" >&2
        false
    fi

    while IFS= read -r binary; do
        [ -n "$binary" ] || continue
        if ! manifest_side_binaries "$manifest" | grep -Fxq "$binary"; then
            obsolete="$INSTALL_DIR_ABS/$binary"
            if managed_side_link "$obsolete"; then
                rm -f "$obsolete"
            fi
        fi
    done <<EOF
$old_side_names
EOF

    side_links_temp=$(mktemp "$SELF_ROOT/.side-links.XXXXXX")
    manifest_side_binaries "$manifest" > "$side_links_temp"
    replace_path "$side_links_temp" "$SELF_ROOT/side-links"

    # Historical OVM versions may have pinned product launchers directly to one
    # immutable version. Repoint only recognized managed symlinks to the control.
    for product in claude codex pi; do
        launcher="$HOME/.ovm/bin/$product"
        if [ -L "$launcher" ] && managed_side_link "$launcher"; then
            switch_link "$launcher" "$control"
        fi
    done

    probe_stdout="$STATE_BACKUP/probe.stdout"
    probe_stderr="$STATE_BACKUP/probe.stderr"
    "$control" self current > "$probe_stdout" 2> "$probe_stderr" &
    probe_pid=$!
    probe_attempts=0
    probe_limit=${OVM_SELF_UPDATE_PROBE_ATTEMPTS:-100}
    while kill -0 "$probe_pid" 2>/dev/null; do
        if [ "$probe_attempts" -ge "$probe_limit" ]; then
            kill "$probe_pid" 2>/dev/null || true
            sleep 0.1
            kill -9 "$probe_pid" 2>/dev/null || true
            wait "$probe_pid" 2>/dev/null || true
            echo "Error: updated OVM control plane activation probe timed out" >&2
            exit 1
        fi
        probe_attempts=$((probe_attempts + 1))
        sleep 0.1
    done
    if wait "$probe_pid"; then
        probe_status=0
    else
        probe_status=$?
    fi
    probe_output=$(cat "$probe_stdout")
    if [ "$probe_status" -ne 0 ] || [ "$probe_output" != "$version" ]; then
        echo "Error: updated OVM control plane failed its activation probe" >&2
        sed -n '1,5p' "$probe_stderr" >&2
        exit 1
    fi
    )
    activation_status=$?
    set -e
    if [ "$activation_status" -ne 0 ]; then
        ROLLBACK_ON_CLEANUP=0
        # restore_install_state now returns non-zero on a partial rollback;
        # call it in a condition so `set -e` can't abort before `fail`, and
        # record the outcome so cleanup keeps the recovery snapshot when the
        # rollback did not fully complete.
        if restore_install_state; then
            fail "OVM activation failed; previous state restored"
        else
            STATE_RESTORE_OK=0
            fail "OVM activation failed; rollback incomplete — recovery snapshot preserved at $STATE_BACKUP"
        fi
    fi
    ROLLBACK_ON_CLEANUP=0

    echo "  Installed version: $version"
    echo "  Active bundle:     $final_dir"
    echo "  Control plane:     $control"
}

if [ -n "$LOCAL_ARTIFACT_DIR" ]; then
    [ -n "$LOCAL_VERSION" ] || fail "OVM_LOCAL_VERSION is required for local artifacts"
    [ -n "$LOCAL_MANIFEST" ] || fail "OVM_LOCAL_MANIFEST is required for local artifacts"
    [ -d "$LOCAL_ARTIFACT_DIR" ] || fail "local artifact directory not found: $LOCAL_ARTIFACT_DIR"
    [ -f "$LOCAL_MANIFEST" ] || fail "local bundle manifest not found: $LOCAL_MANIFEST"
    echo "Installing local OVM snapshot $LOCAL_VERSION..."
    install_bundle "$LOCAL_VERSION" "$LOCAL_MANIFEST" "$LOCAL_ARTIFACT_DIR"
else
    OS=$(uname -s | tr '[:upper:]' '[:lower:]')
    ARCH=$(uname -m)
    case "$OS-$ARCH" in
        darwin-arm64) TARGET="aarch64-apple-darwin" ;;
        darwin-x86_64) TARGET="x86_64-apple-darwin" ;;
        linux-x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
        linux-aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
        *)
            fail "unsupported platform $OS-$ARCH; install from source instead"
            ;;
    esac

    echo "Installing OVM for $TARGET..."
    VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name"' | cut -d'"' -f4)
    [ -n "$VERSION" ] || fail "could not determine latest stable version"
    VERSION_ID=${VERSION#v}

    URL="https://github.com/$REPO/releases/download/$VERSION/$BINARY-$TARGET.tar.gz"
    SHA_URL="$URL.sha256"
    TMP_DIR=$(mktemp -d 2>/dev/null || mktemp -d -t ovm-install)
    ARCHIVE="$TMP_DIR/$BINARY-$TARGET.tar.gz"
    CHECKSUM="$ARCHIVE.sha256"
    EXTRACT_DIR="$TMP_DIR/extract"

    curl -fsSL "$URL" -o "$ARCHIVE"
    curl -fsSL "$SHA_URL" -o "$CHECKSUM"
    expected_sha=$(awk 'NF >= 2 { print $1; exit }' "$CHECKSUM")
    expected_name=$(awk 'NF >= 2 { print $2; exit }' "$CHECKSUM")
    expected_name=${expected_name#\*}
    [ "$expected_name" = "$(basename "$ARCHIVE")" ] || fail "checksum names the wrong archive"
    case "$expected_sha" in
        *[!0-9a-fA-F]*|"") fail "checksum is not a SHA-256 digest" ;;
    esac
    [ "${#expected_sha}" -eq 64 ] || fail "checksum is not a SHA-256 digest"
    actual_sha=$(sha256_file "$ARCHIVE")
    [ "$actual_sha" = "$expected_sha" ] || fail "release archive checksum mismatch"

    validate_archive "$ARCHIVE" "$EXTRACT_DIR"
    install_bundle "$VERSION_ID" "$EXTRACT_DIR/$MANIFEST_NAME" "$EXTRACT_DIR"
fi

echo ""
echo "Add OVM to PATH if needed:"
echo "  export PATH=\"\$HOME/.ovm/bin:\$PATH\""
echo ""
echo "Verify with:"
echo "  ovm --version"
echo "  ovm self current"
