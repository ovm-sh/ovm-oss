#!/usr/bin/env bash
# e2e-command-matrix.sh — install the REAL ovm control plane into an isolated
# HOME and exercise the actual user-facing command + shim surface, asserting
# each dispatches correctly.
#
# Why this exists: the unit/integration suite has ~400 tests but NONE of them
# run the installed control-plane → shim → plugin exec chain. That blind spot
# let a real regression ship (2026-07: every `ccx*` claudex shim and the
# claudex session hook broke because the control plane mis-handled the
# self-managed child marker). This script reproduces the way a user actually
# invokes ovm, so that class of break turns a red build instead of a bug report.
#
# It launches nothing heavyweight (no real claude/codex, no proxy download):
# probes use lightweight subcommands or bounded (EOF) stdin, and assert on
# whether dispatch reached the right binary — not on downstream behaviour.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
TMP=$(mktemp -d)
cleanup() {
  # Every ovm invocation may leave a detached background-refresh child behind,
  # and that child writes into $HOME/.ovm. If one lands mid-delete, `rm -rf`
  # reports "Directory not empty" and, as the trap's last command under
  # `set -e`, turns an already-passing run red. Retry once, then give up
  # quietly: a leftover temp directory is not a test result.
  rm -rf "$TMP" 2>/dev/null && return 0
  sleep 1
  rm -rf "$TMP" 2>/dev/null || true
  return 0
}
trap cleanup EXIT

fail() { echo "E2E FAIL: $*" >&2; exit 1; }
pass() { echo "  ✓ $*"; }
fail_with_log() {
  local label=$1
  local log=$2
  echo "E2E FAIL: $label" >&2
  if [[ -s "$log" ]]; then
    echo "--- captured command output ---" >&2
    cat "$log" >&2
    echo "--- end captured command output ---" >&2
  else
    echo "(command produced no output)" >&2
  fi
  exit 1
}

# OVM-level rejection strings — if any appears, dispatch fell through to the
# top-level ovm CLI instead of reaching the intended command/plugin.
OVM_LEVEL='unexpected argument|unrecognized subcommand|open version manager'

# Every assertion below CAPTURES output first and matches the captured string
# (`grep … <<<"$out"`). Never `some_command | grep -q …` here.
#
# Why: this file runs under `set -o pipefail`. `grep -q` exits the instant it
# matches, which closes the pipe while the producer is still writing. Rust
# ignores SIGPIPE, so ovm's next `println!` fails with EPIPE and panics — exit
# 101 — and pipefail then reports the whole pipeline as failed. The assertion
# fails BECAUSE it matched, and reports "not found". That is exactly how
# `ovm ls codex | grep -q rust-v0.144.0` produced "adopted version not listed"
# on ubuntu CI (2026-08-01) while the version was listed and matched; the race
# is won or lost on scheduling, so the same commit passed and failed.
# A here-string has no producer process, so there is nothing to kill.

# When OVM_E2E_BUNDLE_DIR points at an extracted release bundle (the manifest
# plus its prebuilt ovm/ovm-* binaries), consume those binaries directly and
# skip the from-source build entirely. The alpha canary uses this to prove the
# exact artifacts a user would download rather than a fresh local build.
BUNDLE_DIR="${OVM_E2E_BUNDLE_DIR:-}"

# ---------------------------------------------------------------------------
if [ -z "$BUNDLE_DIR" ]; then
  # Build with the REAL environment (cargo needs its registry/toolchain under
  # the developer's ~/.cargo and ~/.rustup) BEFORE we isolate HOME for install.
  echo "→ building release binaries"
  build_log="$TMP/cargo-build.log"
  if ! ( cd "$ROOT" && cargo build --release ) >"$build_log" 2>&1; then
    fail_with_log "cargo build --release" "$build_log"
  fi

  # dev-install.sh rebuilds via rustup/cargo, which live under the real HOME.
  # Pin them so the toolchain still resolves once HOME is isolated below.
  export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
  export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
fi

# From here on, run hermetically against an isolated HOME so the real ~/.ovm is
# never touched or read.
export HOME="$TMP/home"
mkdir -p "$HOME"
# Deliberately NOT adding ~/.ovm/bin to PATH yet. Every test here used to do
# that on line one — quietly performing the step the installer was supposed to
# perform, and then verifying the result. That is how we shipped an installer
# that left `ovm: command not found` for a real user: the suite handed itself
# the missing piece and confirmed the piece it handed itself. The installer is
# now made to prove it wires PATH on its own, below, before we touch PATH here.
# Strip any ovm bin directory the maintainer already has, or the "fresh" shell
# below would resolve THEIR installed ovm and pass regardless of what the
# installer under test did.
CLEAN_PATH=$(printf '%s' "$PATH" | tr ':' '\n' | grep -v '\.ovm/bin$' | paste -sd: -)
unset OVM_INSTALL_DIR OVM_SELF_MANAGED_CHILD 2>/dev/null || true
# This matrix asserts command dispatch, so the background refresh adds nothing
# but a detached child that touches the network and races teardown. Suppress it
# at the source rather than tolerating the race downstream.
export OVM_BACKGROUND_REFRESH=suppressed

if [ -n "$BUNDLE_DIR" ]; then
  echo "→ installing a prebuilt bundle into an isolated HOME"
  [ -x "$BUNDLE_DIR/ovm" ] || fail "OVM_E2E_BUNDLE_DIR has no executable ovm ($BUNDLE_DIR)"
  BUNDLE_MANIFEST="$BUNDLE_DIR/ovm-bundle-v1.tsv"
  [ -f "$BUNDLE_MANIFEST" ] || fail "OVM_E2E_BUNDLE_DIR has no ovm-bundle-v1.tsv"
  install_log="$TMP/prebuilt-install.log"
  OVM_LOCAL_ARTIFACT_DIR="$BUNDLE_DIR" \
  OVM_LOCAL_MANIFEST="$BUNDLE_MANIFEST" \
  OVM_LOCAL_VERSION="${OVM_E2E_BUNDLE_VERSION:-e2e-prebuilt}" \
  SHELL=/bin/bash \
    sh "$ROOT/install.sh" >"$install_log" 2>&1 \
    || fail_with_log "install prebuilt bundle into isolated HOME" "$install_log"
  # The whole point: a user who runs the documented one-liner and opens a new
  # terminal must be able to type `ovm`. This shell is NOT given PATH by us.
  echo "→ the installer makes ovm findable without help from this test"
  env PATH="$CLEAN_PATH" bash -lc 'command -v ovm >/dev/null 2>&1' \
    || fail_with_log "install.sh left ovm off PATH for a fresh login shell" "$install_log"
  pass "fresh login shell resolves ovm"
else
  echo "→ installing a real snapshot into an isolated HOME"
  install_log="$TMP/dev-install.log"
  OVM_REFRESH_CONTROL=1 "$ROOT/scripts/dev-install.sh" >"$install_log" 2>&1 \
    || fail_with_log "dev-install.sh into isolated HOME" "$install_log"
fi
# The installer's own PATH wiring is proven above for the real-installer path.
# The dev-install path is a developer flow that does not run install.sh, so put
# the bin dir on PATH here for the remaining dispatch assertions.
export PATH="$HOME/.ovm/bin:$PATH"
command -v ovm >/dev/null || fail "ovm not on PATH after install ($HOME/.ovm/bin)"

# ---------------------------------------------------------------------------
echo "→ core self-management"
version_out=$(ovm --version 2>&1) || fail "ovm --version exited non-zero. Got: $version_out"
grep -qiE '^ovm [0-9]' <<<"$version_out" || fail "ovm --version. Got: $version_out"
pass "ovm --version"
current=$(ovm self current 2>&1) || fail "ovm self current failed"
[ -n "$current" ] || fail "ovm self current empty"
pass "ovm self current ($current)"
ovm self list >/dev/null 2>&1 || fail "ovm self list"
pass "ovm self list"
ovm help >/dev/null 2>&1 || fail "ovm help"
pass "ovm help"

# ---------------------------------------------------------------------------
echo "→ claudex launch shims dispatch to the plugin (the ccxy regression class)"
# ccx / ccxy: `<alias> help` reaches claudex's help subcommand (no proxy, no
# network) — a clean positive check that dispatch traversed the full chain.
for alias in ccx ccxy; do
  out=$(ovm "$alias" help 2>&1) || true
  grep -qi 'claudex' <<<"$out" \
    || fail "ovm $alias help did not reach claudex plugin. Got: $out"
  grep -qiE "$OVM_LEVEL" <<<"$out" \
    && fail "ovm $alias fell through to the ovm CLI parser. Got: $out"
  pass "ovm $alias → claudex"
done

# ccxf / ccxyf (fast, and fast+yolo) traverse the IDENTICAL control-plane →
# run_claudex → plugin dispatch chain as ccx/ccxy; they only differ in the flag
# run_claudex injects (--fast prepended, --yolo appended). A runtime probe of
# them would enter claudex setup (proxy download) on a config-less first run,
# so the dispatch regression is guarded by ccx/ccxy above, and the flag
# injection itself is unit-tested in the ovm crate (run_claudex).

# The bare shim is exactly what the user types: `ccxy` → `exec ovm ccxy`.
printf '#!/bin/sh\nexec ovm ccxy "$@"\n' > "$TMP/ccxy"
chmod +x "$TMP/ccxy"
bare_out=$("$TMP/ccxy" help 2>&1) || true
grep -qi 'claudex' <<<"$bare_out" \
  || fail "bare ccxy shim did not reach claudex. resolved ovm: $(command -v ovm). output: $bare_out"
pass "bare ccxy shim → claudex"

# ---------------------------------------------------------------------------
echo "→ claudex session hook survives the self-managed child marker"
# This hook runs from inside a claudex-launched Claude, which carries the
# marker; it must dispatch to the plugin, not error at the ovm CLI.
hook_out=$(printf '{"session_id":"e2e","source":"startup"}' \
  | OVM_SELF_MANAGED_CHILD=1 ovm claudex __session-start 2>&1) || fail "session hook exited non-zero"
grep -qiE "$OVM_LEVEL" <<<"$hook_out" \
  && fail "claudex __session-start fell through to the ovm CLI. Got: $hook_out"
pass "ovm claudex __session-start (marker set)"

# ---------------------------------------------------------------------------
echo "→ adopt imports an existing install without deleting the original"
# `ovm adopt <product> <path>` runs the foreign binary for its --version, then
# imports that managed version and activates it. A real import downloads from
# the release source, which this hermetic script deliberately avoids — so we
# pre-seed a COMPLETE managed codex install and let adopt take the "already
# installed" branch. That still exercises the installed control plane → adopt
# dispatch → version-detection → activation chain and the core safety property:
# the original install is left on disk.
ADOPT_TAG="rust-v0.144.0"
SEED="$HOME/.ovm/products/codex/versions/$ADOPT_TAG/release"
mkdir -p "$SEED/bin"
printf '#!/bin/sh\necho seeded-codex\n' > "$SEED/bin/codex"
chmod +x "$SEED/bin/codex"
: > "$SEED/.complete"
printf '{"version":"%s"}' "$ADOPT_TAG" > "$SEED/meta.json"

# The foreign install: a tiny script whose --version normalizes to $ADOPT_TAG.
printf '#!/bin/sh\necho "codex-cli 0.144.0 (rust-v0.144.0)"\n' > "$TMP/foreign-codex"
chmod +x "$TMP/foreign-codex"

adopt_out=$(ovm adopt codex "$TMP/foreign-codex" 2>&1) || fail "ovm adopt codex. Got: $adopt_out"
grep -qiE "$OVM_LEVEL" <<<"$adopt_out" \
  && fail "ovm adopt fell through to the ovm CLI parser. Got: $adopt_out"
[ -x "$TMP/foreign-codex" ] || fail "adopt deleted the original foreign binary"

ls_out=$(ovm ls codex 2>&1) || fail "ovm ls codex exited non-zero. Got: $ls_out"
grep -Fq "$ADOPT_TAG" <<<"$ls_out" \
  || fail "adopted version not listed by ovm ls codex. Got: $ls_out"
which_out=$(ovm which codex 2>&1) || fail "ovm which codex exited non-zero. Got: $which_out"
grep -Fq "$ADOPT_TAG" <<<"$which_out" \
  || fail "adopted codex not resolvable via ovm which. Got: $which_out"
current_out=$(ovm current codex 2>&1) || fail "ovm current codex exited non-zero. Got: $current_out"
grep -Fq "$ADOPT_TAG" <<<"$current_out" \
  || fail "adopted codex not active via ovm current. Got: $current_out"
pass "ovm adopt codex (original preserved, listed + usable)"

echo "e2e-command-matrix: ok"
