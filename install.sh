#!/bin/sh
# OVM direct installer — installs a verified, self-managed binary bundle.
# Usage: curl -fsSL https://raw.githubusercontent.com/ovm-sh/ovm-oss/main/install.sh | sh
#        … | sh -s -- --claudex   additionally runs the guided claudex setup
set -eu
# Fail a pipeline if any stage fails, not just the last — so a broken `curl` in
# a `curl | grep | cut` can't be masked by a succeeding downstream command.
# pipefail is not POSIX (dash lacks it), so enable it only where supported.
# shellcheck disable=SC3040  # guarded: the subshell probe skips it on POSIX sh
if (set -o pipefail) 2>/dev/null; then set -o pipefail; fi

REPO="ovm-sh/ovm-oss"
BINARY="ovm"
MANIFEST_NAME="ovm-bundle-v1.tsv"
# Download endpoints. These default to the real GitHub hosts; they are
# overridable ONLY so the download branch can be exercised hermetically by
# tests/scripts/install-download-branch.sh. Before the seam existed, every
# automated test supplied OVM_LOCAL_ARTIFACT_DIR and took the local branch, so
# the path a real `curl … | sh` user takes had zero coverage until a release
# was already public. Do not use these to point installs at a mirror: the
# checksum sidecar is fetched from the same host as the archive, so it proves
# integrity, not provenance.
API_BASE="${OVM_INSTALL_API_BASE:-https://api.github.com}"
ASSET_BASE="${OVM_INSTALL_ASSET_BASE:-https://github.com}"
INSTALL_DIR="${OVM_INSTALL_DIR:-$HOME/.ovm/bin}"

# Options. The installer stays zero-argument for the plain path; --claudex
# chains into the guided claudex onboarding after a successful install.
#
# --version installs an EXACT tag instead of the latest stable. GitHub's
# releases/latest endpoint excludes prereleases by definition, so without this
# there is no way for the public one-liner to install an alpha at all — which
# is what a clean-machine rehearsal of a release candidate needs.
#
#   curl -fsSL https://ovm.sh/install | sh -s -- --version v0.1.8-alpha.1
CLAUDEX_SETUP=0
REQUESTED_VERSION="${OVM_INSTALL_VERSION:-}"
usage() {
    echo "Usage: curl -fsSL https://ovm.sh/install | sh -s -- [--claudex] [--version <tag>]" >&2
}
while [ $# -gt 0 ]; do
    case "$1" in
        --claudex) CLAUDEX_SETUP=1 ;;
        --version)
            shift
            [ $# -gt 0 ] || { echo "--version needs a release tag" >&2; usage; exit 2; }
            REQUESTED_VERSION="$1"
            ;;
        --version=*) REQUESTED_VERSION="${1#--version=}" ;;
        *)
            echo "Unknown option: $1" >&2
            usage
            exit 2
            ;;
    esac
    shift
done
# The tag goes straight into a URL path, so it may hold only what a git tag
# legitimately holds. Anything else is a caller error, not a request to fetch.
case "$REQUESTED_VERSION" in
    "") ;;
    *[!A-Za-z0-9._+-]*)
        echo "Not a release tag: $REQUESTED_VERSION" >&2
        exit 2
        ;;
esac
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
LOCK_WAITING=""
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
    release_operation_lock
    exit "$cleanup_status"
}

# Idempotent: every branch guards on state, so cleanup can call it after an
# explicit early release. The lock must not outlive the install itself — the
# --claudex chain can hand the terminal to a long-lived session, and holding
# the self-operation lock across it would block every `ovm self` op meanwhile.
release_operation_lock() {
    if [ "$LOCK_PIPE_OPEN" = "1" ]; then
        exec 9>&-
        LOCK_PIPE_OPEN=0
    fi
    if [ -n "$LOCK_HELPER_PID" ]; then
        kill "$LOCK_HELPER_PID" 2>/dev/null || true
        wait "$LOCK_HELPER_PID" 2>/dev/null || true
        LOCK_HELPER_PID=""
    fi
    if [ -n "$LOCK_FIFO" ]; then
        rm -f "$LOCK_FIFO"
        LOCK_FIFO=""
    fi
    if [ -n "$LOCK_READY" ]; then
        rm -f "$LOCK_READY"
        LOCK_READY=""
    fi
    if [ -n "$LOCK_WAITING" ]; then
        rm -f "$LOCK_WAITING"
        LOCK_WAITING=""
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Mochi the Cat — the same mascot the installed CLI greets with (see
# crates/ovm/src/mochi.rs). Message rides the cat's middle line. The fur is
# ANSI magenta — the terminal theme's purple, matching the CLI's fur — when
# stderr is a real terminal; plain otherwise.
if [ -t 2 ] && [ "${TERM:-dumb}" != "dumb" ]; then
    MOCHI_FUR=$(printf '\033[35m')
    MOCHI_RESET=$(printf '\033[0m')
else
    MOCHI_FUR=""
    MOCHI_RESET=""
fi

mochi() {
    mood=$1
    shift
    case "$mood" in
        happy) eyes='^.^' ;;
        sad) eyes='u.u' ;;
        working) eyes='-.-' ;;
        *) eyes='o.o' ;;
    esac
    printf '\n%s  /\\_/\\ %s\n%s ( %s )%s  %s\n%s  > ^ < %s\n' \
        "$MOCHI_FUR" "$MOCHI_RESET" \
        "$MOCHI_FUR" "$eyes" "$MOCHI_RESET" "$*" \
        "$MOCHI_FUR" "$MOCHI_RESET" >&2
}

fail() {
    mochi sad "Error: $*"
    exit 1
}

# Download with retries.
#
# A bare `curl -fsSL` fails the whole install on one transient blip — a real run
# died on `HTTP/2 stream 1 was not closed cleanly: PROTOCOL_ERROR` partway
# through, which is a CDN hiccup, not a broken release. Retry transient
# failures; a genuinely missing asset still fails fast because 404 is not
# retried without --retry-all-errors.
#
# --retry-all-errors is curl 7.71+, so probe once rather than assuming: older
# curl treats an unknown flag as a hard usage error.
CURL_RETRY_FLAGS=
detect_curl_retry_flags() {
    [ -z "$CURL_RETRY_FLAGS" ] || return 0
    CURL_RETRY_FLAGS="--retry 3 --retry-delay 1 --retry-connrefused"
    # No pipe: `curl --help all | grep -q` is the inverted-pipeline shape —
    # grep exits at the match, curl takes SIGPIPE, and under pipefail the probe
    # reports "unsupported" precisely when the flag IS supported. POSIX sh has
    # no here-strings, so capture and pattern-match instead.
    curl_help=$(curl --help all 2>/dev/null || true)
    case "$curl_help" in
        *--retry-all-errors*)
            CURL_RETRY_FLAGS="$CURL_RETRY_FLAGS --retry-all-errors"
            ;;
    esac
}

fetch() {
    detect_curl_retry_flags
    # Word-splitting is intended: CURL_RETRY_FLAGS is a flag list we built.
    # shellcheck disable=SC2086
    curl -fsSL $CURL_RETRY_FLAGS "$@"
}

# Paths shown to the user are printed with ~ rather than the expanded home:
# they read the same on every machine and never put a username on screen (or
# in a pasted log).
display_path() {
    # shellcheck disable=SC2088  # the literal ~ is the point: display only
    case "$1" in
        "$HOME"/*) printf '~/%s\n' "${1#"$HOME"/}" ;;
        *) printf '%s\n' "$1" ;;
    esac
}

# --- PATH wiring -----------------------------------------------------------
# Printing "add this to your PATH if needed" and doing nothing left a working
# install that the shell could not find, which reads as a failed install. Wire
# it up, idempotently, and say exactly what changed.

OVM_PATH_BEGIN='# >>> ovm >>>'
OVM_PATH_END='# <<< ovm <<<'

path_already_has() {
    # Exact element match, so /opt/ovm/bin does not count as $HOME/.ovm/bin.
    case ":$PATH:" in
        *":$1:"*) return 0 ;;
        *) return 1 ;;
    esac
}

# Append the block to one rc file unless our marker is already there. Echoes
# the path when it writes, so the caller can report what it touched.
write_path_block() {
    file=$1
    line=$2
    [ -n "$file" ] || return 0
    if [ -f "$file" ] && grep -Fq "$OVM_PATH_BEGIN" "$file" 2>/dev/null; then
        return 0
    fi
    parent=$(dirname "$file")
    mkdir -p "$parent" 2>/dev/null || return 0
    # A read-only rc file, or one in a directory we cannot write, is the user's
    # business — but check BOTH, or the append leaks a raw "Permission denied"
    # from the redirect before any handling can run.
    if [ -e "$file" ]; then
        [ -w "$file" ] || return 0
    else
        [ -w "$parent" ] || return 0
    fi
    {
        printf '\n%s\n' "$OVM_PATH_BEGIN"
        printf '%s\n' "$line"
        printf '%s\n' "$OVM_PATH_END"
    } >> "$file" 2>/dev/null || return 0
    printf '%s\n' "$file"
}

# Append our PATH block to one rc file, recording which of the two things
# happened: appended (`written`) or already there (`existing`). write_path_block
# is deliberately silent in both the "already present" and "cannot write" cases,
# so the caller cannot tell them apart from its output alone.
wire_rc() {
    file=$1
    line=$2
    if [ -f "$file" ] && grep -Fq "$OVM_PATH_BEGIN" "$file" 2>/dev/null; then
        existing="$existing $file"
        return 0
    fi
    got=$(write_path_block "$file" "$line") || true
    [ -n "$got" ] && written="$written $got"
    return 0
}

configure_path() {
    install_dir=$1
    # Write $HOME symbolically so the rc line survives a moved home directory.
    case "$install_dir" in
        "$HOME"/*) path_line_dir="\$HOME/${install_dir#"$HOME"/}" ;;
        *) path_line_dir=$install_dir ;;
    esac

    PATH_ACTION_LINE="export PATH=\"$path_line_dir:\$PATH\""

    if path_already_has "$install_dir"; then
        PATH_OUTRO_KIND=already
        return 0
    fi

    if [ -n "${OVM_NO_MODIFY_PATH:-}" ]; then
        PATH_OUTRO_KIND=not_modified
        return 0
    fi

    shell_name=$(basename "${SHELL:-sh}")
    written=""
    existing=""
    case "$shell_name" in
        zsh)
            # .zshrc covers interactive shells, .zprofile covers login shells —
            # `zsh -lc` reads only the latter, and a PATH that works in one but
            # not the other is its own confusing bug report.
            for rc in "${ZDOTDIR:-$HOME}/.zshrc" "${ZDOTDIR:-$HOME}/.zprofile"; do
                wire_rc "$rc" "export PATH=\"$path_line_dir:\$PATH\""
            done
            ;;
        bash)
            for rc in "$HOME/.bashrc" "$HOME/.bash_profile"; do
                wire_rc "$rc" "export PATH=\"$path_line_dir:\$PATH\""
            done
            ;;
        fish)
            wire_rc "$HOME/.config/fish/conf.d/ovm.fish" "fish_add_path $install_dir"
            ;;
        *)
            wire_rc "$HOME/.profile" "export PATH=\"$path_line_dir:\$PATH\""
            ;;
    esac

    if [ -z "$written" ]; then
        # Nothing was appended, for one of two very different reasons — and
        # conflating them told ordinary upgraders their install was broken.
        #
        # The block may ALREADY be in the rc, which is the common shape: run
        # the one-liner again from the same shell you installed in, and PATH
        # still lacks the directory while the rc has carried the line all
        # along. Nothing is wrong; a new terminal works. Saying "your shell
        # config could not be written" there is simply false.
        if [ -n "$existing" ]; then
            PATH_WRITTEN=$existing
            PATH_OUTRO_KIND=already_wired
            return 0
        fi
        # Or the files really could not be written. OVM on disk that the shell
        # cannot find is precisely the state users report as a failed install,
        # so this must never read as a clean one.
        PATH_OUTRO_KIND=failed
        return 0
    fi

    # Recorded, not printed. The hatch below runs between here and the end of
    # the install, and its first screen clears the terminal — advice printed
    # now is advice the reader never gets to read.
    PATH_WRITTEN=$written
    PATH_OUTRO_KIND=written
}

# Whether the shell that launched this installer will find `ovm` afterwards.
# Only `already` and a successful rc write leave a usable shell behind, and a
# written rc helps NEW shells only — a child cannot alter its parent's PATH.
path_is_pending() {
    case "${PATH_OUTRO_KIND:-none}" in
        already) return 1 ;;
        *) return 0 ;;
    esac
}

# The "what now", printed last so nothing can scroll or clear it away.
#
# Stands down when the hatch ran AND the rc write worked: the tour closes with
# its own version, naming the shortcuts it just installed, and two closing
# screens disagreeing about what to type next is worse than either alone. When
# the rc write did NOT happen, the tour's "open a new terminal" is not true —
# a new terminal will not have it either — so this prints regardless and the
# accurate warning gets the last word.
print_path_outro() {
    if [ "${HATCH_RAN:-0}" = 1 ]; then
        case "${PATH_OUTRO_KIND:-none}" in
            already|written) return 0 ;;
        esac
    fi
    echo ""
    echo "Verify with:"
    echo "  ovm --version"
    echo "  ovm self current"
    echo ""
    case "${PATH_OUTRO_KIND:-none}" in
        already)
            echo "OVM is on your PATH already."
            ;;
        already_wired)
            echo "OVM was already in your PATH configuration:"
            for file in $PATH_WRITTEN; do
                echo "  $(display_path "$file")"
            done
            echo ""
            echo "This shell predates it. Open a new terminal, or run here:"
            echo "  $PATH_ACTION_LINE"
            ;;
        written)
            echo "Added OVM to your PATH in:"
            for file in $PATH_WRITTEN; do
                echo "  $(display_path "$file")"
            done
            echo ""
            echo "Open a new terminal, or run this once in this one:"
            echo "  $PATH_ACTION_LINE"
            ;;
        not_modified)
            echo "OVM_NO_MODIFY_PATH is set, so your shell config was left alone."
            echo "Add this yourself, or ovm will not be found in any terminal:"
            echo "  $PATH_ACTION_LINE"
            ;;
        failed)
            echo "WARNING: OVM is installed but NOT on your PATH."
            echo ""
            echo "Your shell config could not be written, so you must add this line"
            echo "yourself or ovm will not be found in a new terminal:"
            echo "  $PATH_ACTION_LINE"
            ;;
    esac
    echo ""
    echo "There's a story behind the cats — meet them:  ovm hatch"
    echo ""
}
# --- end PATH wiring -------------------------------------------------------

acquire_operation_lock() {
    helper=$1
    mkdir -p "$SELF_ROOT"
    LOCK_FIFO="$SELF_ROOT/.operation-lock-fifo.$$"
    LOCK_READY="$SELF_ROOT/.operation-lock-ready.$$"
    LOCK_WAITING="$LOCK_READY.waiting"
    rm -f "$LOCK_FIFO" "$LOCK_READY" "$LOCK_WAITING"
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
        # Announce a wait only when the helper says it is waiting: it writes
        # this marker after the lock refuses a non-blocking take, i.e. only
        # when another operation really holds it. Elapsed time cannot tell the
        # two apart — the helper is a cold exec of a freshly extracted binary,
        # and on a first run that alone outlasts any threshold worth setting,
        # which is how a clean install came to announce a wait for nothing.
        if [ "$announced" = "0" ] && [ -f "$LOCK_WAITING" ]; then
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

    # Capture the manifest's side names once and match against the captured
    # text. `manifest_side_binaries "$manifest" | grep -Fxq "$binary"` inverts:
    # grep -q exits at the first match, the awk producer's next write dies with
    # EPIPE, and pipefail reports the pipeline as FAILED exactly when the name
    # IS in the manifest — so `!` fires and a live side link is deleted as
    # obsolete. It only bites once the manifest outgrows the pipe buffer, which
    # is why it survived review. The capture also stops re-running awk once per
    # installed binary.
    kept_side_names=$(manifest_side_binaries "$manifest")
    while IFS= read -r binary; do
        [ -n "$binary" ] || continue
        if grep -Fxq "$binary" <<INNER
$kept_side_names
INNER
        then
            continue
        fi
        obsolete="$INSTALL_DIR_ABS/$binary"
        if managed_side_link "$obsolete"; then
            rm -f "$obsolete"
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
    echo "  Active bundle:     $(display_path "$final_dir")"
    echo "  Control plane:     $(display_path "$control")"
}

if [ -n "$LOCAL_ARTIFACT_DIR" ]; then
    [ -n "$LOCAL_VERSION" ] || fail "OVM_LOCAL_VERSION is required for local artifacts"
    [ -n "$LOCAL_MANIFEST" ] || fail "OVM_LOCAL_MANIFEST is required for local artifacts"
    [ -d "$LOCAL_ARTIFACT_DIR" ] || fail "local artifact directory not found: $LOCAL_ARTIFACT_DIR"
    [ -f "$LOCAL_MANIFEST" ] || fail "local bundle manifest not found: $LOCAL_MANIFEST"
    mochi working "Installing local OVM snapshot $LOCAL_VERSION..."
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

    # The published Linux binaries are GNU builds with a glibc 2.35 floor
    # (the release builders run Ubuntu 22.04). Check BEFORE downloading:
    # without this, installation on an older or musl libc completes cleanly
    # and the first `ovm` invocation dies in the dynamic loader — an install
    # that "succeeded" into a broken state. musl systems (Alpine) and
    # pre-2.35 glibc (Ubuntu 20.04, RHEL 9) must build from source instead.
    #
    # This guard fails CLOSED. An unidentifiable libc used to be waved
    # through "because the loader will say no" — but the loader says no
    # AFTER the install reports success, which is the exact broken state the
    # check exists to prevent. A system with no getconf and no ldd is a
    # system we cannot qualify, so it is refused with the escape hatch named
    # in the message.
    if [ "$OS" = "linux" ] && [ "${OVM_SKIP_LIBC_CHECK:-}" != "1" ]; then
        GLIBC_FLOOR="2.35"
        libc_report=$(getconf GNU_LIBC_VERSION 2>/dev/null || true)
        if [ -z "$libc_report" ]; then
            libc_report=$(ldd --version 2>&1 | head -n1 || true) # pipefail-safe: `|| true` discards the pipeline status outright, and an absent report is handled as "unidentifiable" below
        fi
        case "$libc_report" in
            *musl*)
                fail "OVM's prebuilt Linux binaries need glibc; this system uses musl libc. Install from source instead."
                ;;
        esac
        # Only a line that actually names glibc is parsed for a version.
        # Matching any "x.y" anywhere took the first number in the line, so a
        # tool-version banner ("BusyBox v1.36.1 …") was read as "glibc 1.36"
        # and rejected the system with a wrong reason. glibc's own reports —
        # "glibc 2.35" from getconf, "ldd (Ubuntu GLIBC 2.35-0ubuntu3) 2.35"
        # from ldd — both end in the bare version, so take the last field.
        glibc_version=""
        case "$libc_report" in
            *glibc*|*GLIBC*|*"GNU libc"*|*"GNU C Library"*)
                glibc_version=$(printf '%s\n' "$libc_report" | awk '{print $NF}')
                ;;
        esac
        case "$glibc_version" in
            ''|*[!0-9.]*) glibc_version="" ;;
            *.*) ;;
            *) glibc_version="" ;;
        esac
        if [ -z "$glibc_version" ]; then
            fail "could not identify this system's C library, so OVM cannot tell whether its prebuilt binaries will run here (getconf/ldd reported: ${libc_report:-nothing}).
       The binaries need glibc >= $GLIBC_FLOOR. Install from source instead, or re-run with OVM_SKIP_LIBC_CHECK=1 to install anyway."
        fi
        lowest=$(printf '%s\n%s\n' "$GLIBC_FLOOR" "$glibc_version" | sort -V | head -n1) # pipefail-safe: sort consumes all input before emitting, so head cannot close the pipe on two lines
        if [ "$lowest" != "$GLIBC_FLOOR" ]; then
            fail "OVM's prebuilt Linux binaries need glibc >= $GLIBC_FLOOR (Ubuntu 22.04+, Debian 12+, Fedora 36+); this system has glibc $glibc_version. Install from source instead."
        fi
    fi

    mochi working "Installing OVM for $TARGET..."
    # Split the fetch from the parse. Folded into one pipeline, a failed curl
    # aborted the whole script under `set -e` before the `fail` below could
    # run, so an unreachable API exited silently with curl's status and no
    # explanation — the user saw nothing at all.
    # `fetch` reports failure without a status, so a 404 and an unreachable
    # host arrive identically. Rather than guess, name the tag and both
    # possibilities — the tag is the actionable half either way.
    # Two different failures, kept apart. `fetch` reports failure without a
    # status, so a 404 and an unreachable host arrive identically — name the
    # tag and both possibilities, since the tag is the actionable half either
    # way. A response that ARRIVES but carries no tag_name is a third thing
    # again, and saying "could not reach" about it would be false.
    if [ -n "$REQUESTED_VERSION" ]; then
        release_ref="releases/tags/$REQUESTED_VERSION"
        release_unavailable="no OVM release tagged $REQUESTED_VERSION at $API_BASE — check the tag is published, or drop --version for the latest stable"
        release_untagged="the release tagged $REQUESTED_VERSION names no version"
    else
        release_ref="releases/latest"
        release_unavailable="could not reach $API_BASE to look up the latest OVM release"
        release_untagged="could not determine latest stable version"
    fi
    release_json=$(fetch "$API_BASE/repos/$REPO/$release_ref") || fail "$release_unavailable"
    VERSION=$(printf '%s\n' "$release_json" | grep '"tag_name"' | cut -d'"' -f4 || true)
    [ -n "$VERSION" ] || fail "$release_untagged"
    VERSION_ID=${VERSION#v}

    URL="$ASSET_BASE/$REPO/releases/download/$VERSION/$BINARY-$TARGET.tar.gz"
    SHA_URL="$URL.sha256"
    TMP_DIR=$(mktemp -d 2>/dev/null || mktemp -d -t ovm-install)
    ARCHIVE="$TMP_DIR/$BINARY-$TARGET.tar.gz"
    CHECKSUM="$ARCHIVE.sha256"
    EXTRACT_DIR="$TMP_DIR/extract"

    # Same reason as the release lookup above: name the asset that is missing
    # rather than dying on curl's exit status with an empty terminal.
    fetch "$URL" -o "$ARCHIVE" ||
        fail "could not download the release archive $URL"
    fetch "$SHA_URL" -o "$CHECKSUM" ||
        fail "could not download the release checksum $SHA_URL"
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

mochi happy "OVM is installed. Happy shipping!"
echo ""
configure_path "$INSTALL_DIR"

# Hatch on fresh machines. Same two rules as the claudex chain below: the
# install is COMPLETE before any of this runs (release the lock first — the
# hatch can hand the terminal to long-lived sessions), and a declined or
# failed hatch never fails the installer. Fresh means no managed products yet:
# a returning user upgrading OVM has chosen their setup already and gets the
# story pointer above, not a prompt. `curl | sh` without a terminal skips
# silently — the hatch itself also refuses non-tty, but the prompt must not
# try to read an answer from the pipe. Skipped when --claudex was asked for:
# that flag IS a chosen onboarding path.
if [ "$CLAUDEX_SETUP" != 1 ] && [ ! -d "$HOME/.ovm/products" ] && (exec < /dev/tty) 2>/dev/null; then
    echo ""
    printf "Hatch your setup now? Claude Code, Codex and claudex, with the story. [Y/n] "
    hatch_answer=""
    IFS= read -r hatch_answer < /dev/tty || hatch_answer="n"
    case "$hatch_answer" in
        [Nn]*) echo "Anytime later:  ovm hatch" ;;
        *)
            release_operation_lock
            HATCH_RAN=1
            # OVM_PATH_PENDING says what the injected PATH below hides: this
            # installer's own shell got the rc-file line, the shell that
            # launched it did not, and no child can fix that for its parent.
            HATCH_PATH_PENDING=""
            if path_is_pending; then
                HATCH_PATH_PENDING=1
            fi
            OVM_PATH_PENDING="$HATCH_PATH_PENDING" \
            PATH="$INSTALL_DIR:$PATH" "$INSTALL_DIR/$BINARY" hatch < /dev/tty || {
                echo ""
                echo "The hatch did not finish — the OVM install itself succeeded."
                echo "Pick it back up anytime with:  ovm hatch"
            }
            ;;
    esac
fi

if [ "$CLAUDEX_SETUP" = 1 ]; then
    echo ""
    # Guided claudex onboarding. The install above is COMPLETE, so two rules:
    # release the self-operation lock first (setup can hand the terminal to a
    # long-lived session, and `ovm self` ops must not block behind it), and
    # never let a setup failure fail the installer — a declined OAuth is not a
    # broken install. Under `curl … | sh` stdin is the pipe, which can't
    # answer prompts — re-attach the terminal when there is one. The probe
    # actually OPENS /dev/tty: on CI runners and in containers the node exists
    # but has no controlling terminal, so `-r` alone would pass and the
    # redirect below would then kill a script whose install already succeeded.
    release_operation_lock
    if (exec < /dev/tty) 2>/dev/null; then
        # configure_path updates future shells, not this installer process. Put
        # the fresh control plane on PATH for setup and every child it launches
        # (`ovm install`, `ovm use`, and the generated shims all invoke `ovm`).
        PATH="$INSTALL_DIR:$PATH" "$INSTALL_DIR/$BINARY" claudex setup < /dev/tty || {
            echo ""
            echo "The guided claudex setup did not finish — the OVM install itself succeeded."
            echo "Pick it back up anytime with:  ovm claudex setup"
        }
    else
        echo "No terminal available for the guided claudex setup."
        echo "Run it when you have one:  ovm claudex setup"
    fi
fi

print_path_outro
