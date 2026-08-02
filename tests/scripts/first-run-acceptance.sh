#!/usr/bin/env bash
# First-run acceptance: be a brand-new user and see if OVM actually works.
#
# WHY THIS EXISTS
# ---------------
# On 2026-08-01 a real user ran the documented one-liner, the install succeeded,
# and their shell said `zsh: command not found: ovm`. Nine rounds of review and
# a full CI suite had not caught it, because every test and the release canary
# began by exporting PATH="$HOME/.ovm/bin:$PATH" themselves — quietly performing
# the step the installer was supposed to perform, then verifying the result.
# We proved the binary worked. We never once asked whether a person could type
# its name.
#
# THE RULE THIS TEST ENFORCES
# ---------------------------
# Nothing here may hand OVM a precondition that a real user's machine would not
# hand it. No PATH export, no absolute paths to the binary, no pre-seeded shell
# config. If a step needs something, the INSTALLER must have provided it.
#
# Set OVM_ACCEPTANCE_REMOTE=1 to fetch the installer from the live site instead
# of the working copy, which additionally proves what ovm.sh actually serves.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
TMP=$(mktemp -d)
cleanup() {
  # A detached background-refresh child can be writing under $HOME as we delete.
  rm -rf "$TMP" 2>/dev/null && return 0
  sleep 1
  rm -rf "$TMP" 2>/dev/null || true
  return 0
}
trap cleanup EXIT

fail() { echo "FIRST-RUN FAIL: $*" >&2; exit 1; }
pass() { echo "  ✓ $*"; }

INSTALLER="$ROOT/install.sh"
if [ "${OVM_ACCEPTANCE_REMOTE:-}" = "1" ]; then
  echo "→ fetching the installer from https://ovm.sh/install"
  INSTALLER="$TMP/install-remote.sh"
  curl -fsSL https://ovm.sh/install -o "$INSTALLER" \
    || fail "could not fetch https://ovm.sh/install — that URL is the documented entry point"
  [ -s "$INSTALLER" ] || fail "https://ovm.sh/install served an empty body"
  pass "ovm.sh/install is reachable and non-empty"
fi

# PUBLISHED mode (post-release): install nothing local — let the real installer
# fetch the real published release, exactly as a user's machine would. Set
# OVM_ACCEPTANCE_PUBLISHED to the version string the install must land.
PUBLISHED="${OVM_ACCEPTANCE_PUBLISHED:-}"
MANIFEST_SRC="$ROOT/crates/ovm/ovm-bundle-v1.tsv"
LOCAL_ENV=()

if [ -n "$PUBLISHED" ]; then
  echo "→ published mode: the installer will download $PUBLISHED itself"
else
  # --- build a bundle so the installer runs offline against real binaries ---
  echo "→ building release binaries"
  BUNDLE="$TMP/bundle"
  mkdir -p "$BUNDLE"
  build_log="$TMP/build.log"
  ( cd "$ROOT" && cargo build --release --quiet ) >"$build_log" 2>&1 \
    || { cat "$build_log" >&2; fail "cargo build --release"; }
  [ -f "$MANIFEST_SRC" ] || fail "missing bundle manifest at $MANIFEST_SRC"
  cp "$MANIFEST_SRC" "$BUNDLE/ovm-bundle-v1.tsv"
  while IFS= read -r binary; do
    [ -n "$binary" ] || continue
    [ -f "$ROOT/target/release/$binary" ] || fail "release build produced no $binary"
    cp "$ROOT/target/release/$binary" "$BUNDLE/$binary"
  done < <(sh "$ROOT/scripts/bundle-manifest.sh" binaries "$MANIFEST_SRC")
  LOCAL_ENV=(
    "OVM_LOCAL_ARTIFACT_DIR=$BUNDLE"
    "OVM_LOCAL_MANIFEST=$BUNDLE/ovm-bundle-v1.tsv"
    "OVM_LOCAL_VERSION=acceptance-0.0.0"
  )
  pass "bundle assembled"
fi

# --- a machine with no OVM anywhere ----------------------------------------
export HOME="$TMP/home"
mkdir -p "$HOME"
# The user's PATH. Note what is NOT here: ~/.ovm/bin. If a later step finds
# ovm, it is because the installer earned it.
VIRGIN_PATH="/usr/bin:/bin:/usr/sbin:/sbin"
command -v bash >/dev/null || fail "this test needs bash"

echo "→ running the installer exactly as the website tells a user to"
install_log="$TMP/install.log"
env -i \
  HOME="$HOME" \
  PATH="$VIRGIN_PATH" \
  SHELL=/bin/bash \
  TERM="${TERM:-dumb}" \
  ${LOCAL_ENV[@]+"${LOCAL_ENV[@]}"} \
  sh "$INSTALLER" >"$install_log" 2>&1 \
  || { cat "$install_log" >&2; fail "the installer itself failed"; }
pass "installer completed"

# The installer must say what it did to the user's shell. Silence here is how
# somebody ends up believing the install failed.
grep -qiE 'PATH' "$install_log" \
  || fail "the installer said nothing about PATH; a user has no idea what to do next"
pass "installer explained the PATH outcome"

# --- the actual bug: can a new shell find it? -------------------------------
echo "→ a fresh shell, given nothing by this test, must find ovm"

run_in_fresh_shell() {
  # env -i so nothing leaks in from the test's own environment.
  env -i HOME="$HOME" PATH="$VIRGIN_PATH" SHELL=/bin/bash TERM=dumb \
    OVM_BACKGROUND_REFRESH=suppressed \
    bash "$@"
}

run_in_fresh_shell -lc 'command -v ovm >/dev/null' \
  || fail "login shell cannot find ovm — this is the 'command not found' regression"
pass "login shell (bash -lc) resolves ovm"

run_in_fresh_shell -ic 'command -v ovm >/dev/null' 2>/dev/null \
  || fail "interactive shell cannot find ovm (rc file wired, profile missed, or vice versa)"
pass "interactive shell (bash -ic) resolves ovm"

# --- and can they actually use it? ------------------------------------------
echo "→ the commands a new user would try first"

# Every assertion below CAPTURES the output first and matches the captured
# string (`grep … <<<"$out"`). Never `echo "$out" | grep -q …` here: this file
# runs under `set -o pipefail`, `grep -q` exits on its first match, and the
# producer's next write then fails with EPIPE — so pipefail reports the
# pipeline as failed and the assertion fails BECAUSE it matched. That is how
# `ovm ls codex | grep -q rust-v0.144.0` reported "adopted version not listed"
# on ubuntu CI (2026-08-01) for a version that was listed.
version=$(run_in_fresh_shell -lc 'ovm --version' 2>&1) \
  || fail "ovm --version failed: $version"
grep -qiE '^ovm [0-9]' <<<"$version" || fail "ovm --version printed: $version"
pass "ovm --version ($version)"

current=$(run_in_fresh_shell -lc 'ovm self current' 2>&1) \
  || fail "ovm self current failed: $current"
[ -n "$current" ] || fail "ovm self current printed nothing"
pass "ovm self current ($current)"
if [ -n "$PUBLISHED" ]; then
  [ "$current" = "$PUBLISHED" ] \
    || fail "the public installer landed '$current', expected '$PUBLISHED'"
  pass "the public installer landed the released version"
fi

run_in_fresh_shell -lc 'ovm help >/dev/null' || fail "ovm help failed"
pass "ovm help"

# A brand-new install has no products yet, so these must report an empty state
# rather than erroring — "nothing installed" is a valid answer, not a failure.
overview=$(run_in_fresh_shell -lc 'ovm current' 2>&1) \
  || fail "ovm current failed on a fresh install: $overview"
grep -q 'not installed' <<<"$overview" \
  || fail "ovm current did not report the products as uninstalled: $overview"
pass "ovm current reports an empty install"

run_in_fresh_shell -lc 'ovm which >/dev/null' \
  || fail "ovm which failed on a fresh install"
pass "ovm which"

listing=$(run_in_fresh_shell -lc 'ovm ls claude' 2>&1) \
  || fail "ovm ls claude failed on a fresh install: $listing"
grep -qi 'ovm install' <<<"$listing" \
  || fail "an empty listing must tell the user how to install, got: $listing"
pass "ovm ls claude points a new user at the next step"

# Side commands ship in the same bundle and share the PATH entry; if the bin
# directory is wired at all they must all be reachable.
while IFS= read -r binary; do
  [ -n "$binary" ] || continue
  [ "$binary" = "ovm" ] && continue
  run_in_fresh_shell -lc "command -v '$binary' >/dev/null" \
    || fail "$binary shipped in the bundle but is not on the user's PATH"
done < <(sh "$ROOT/scripts/bundle-manifest.sh" binaries "$MANIFEST_SRC")
pass "bundled side commands are reachable"

# --- installing twice must not corrupt anything -----------------------------
echo "→ re-running the installer (users do this to upgrade)"
env -i HOME="$HOME" PATH="$VIRGIN_PATH" SHELL=/bin/bash TERM="${TERM:-dumb}" \
  ${LOCAL_ENV[@]+"${LOCAL_ENV[@]}"} \
  sh "$INSTALLER" >"$TMP/install2.log" 2>&1 \
  || { cat "$TMP/install2.log" >&2; fail "second install failed"; }
run_in_fresh_shell -lc 'ovm --version >/dev/null' \
  || fail "ovm stopped working after a second install"
blocks=$(grep -c '>>> ovm >>>' "$HOME/.bashrc" 2>/dev/null || echo 0)
[ "$blocks" -le 1 ] || fail "re-running the installer stacked $blocks PATH blocks in .bashrc"
pass "re-install is idempotent"

echo "first-run-acceptance: ok"
