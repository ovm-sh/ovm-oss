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
trap 'rm -rf "$TMP"' EXIT

fail() { echo "E2E FAIL: $*" >&2; exit 1; }
pass() { echo "  ✓ $*"; }

# OVM-level rejection strings — if any appears, dispatch fell through to the
# top-level ovm CLI instead of reaching the intended command/plugin.
OVM_LEVEL='unexpected argument|unrecognized subcommand|open version manager'

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
  ( cd "$ROOT" && cargo build --release >/dev/null 2>&1 ) || fail "cargo build --release"

  # dev-install.sh rebuilds via rustup/cargo, which live under the real HOME.
  # Pin them so the toolchain still resolves once HOME is isolated below.
  export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
  export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
fi

# From here on, run hermetically against an isolated HOME so the real ~/.ovm is
# never touched or read.
export HOME="$TMP/home"
mkdir -p "$HOME"
export PATH="$HOME/.ovm/bin:$PATH"
unset OVM_INSTALL_DIR OVM_SELF_MANAGED_CHILD 2>/dev/null || true

if [ -n "$BUNDLE_DIR" ]; then
  echo "→ installing a prebuilt bundle into an isolated HOME"
  [ -x "$BUNDLE_DIR/ovm" ] || fail "OVM_E2E_BUNDLE_DIR has no executable ovm ($BUNDLE_DIR)"
  BUNDLE_MANIFEST="$BUNDLE_DIR/ovm-bundle-v1.tsv"
  [ -f "$BUNDLE_MANIFEST" ] || fail "OVM_E2E_BUNDLE_DIR has no ovm-bundle-v1.tsv"
  OVM_LOCAL_ARTIFACT_DIR="$BUNDLE_DIR" \
  OVM_LOCAL_MANIFEST="$BUNDLE_MANIFEST" \
  OVM_LOCAL_VERSION="${OVM_E2E_BUNDLE_VERSION:-e2e-prebuilt}" \
    sh "$ROOT/install.sh" >/dev/null 2>&1 \
    || fail "install prebuilt bundle into isolated HOME"
else
  echo "→ installing a real snapshot into an isolated HOME"
  OVM_REFRESH_CONTROL=1 "$ROOT/scripts/dev-install.sh" >/dev/null 2>&1 \
    || fail "dev-install.sh into isolated HOME"
fi
command -v ovm >/dev/null || fail "ovm not on PATH after install ($HOME/.ovm/bin)"

# ---------------------------------------------------------------------------
echo "→ core self-management"
ovm --version 2>&1 | grep -qiE '^ovm [0-9]' || fail "ovm --version"
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
  echo "$out" | grep -qi 'claudex' \
    || fail "ovm $alias help did not reach claudex plugin. Got: $out"
  echo "$out" | grep -qiE "$OVM_LEVEL" \
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
echo "$bare_out" | grep -qi 'claudex' \
  || fail "bare ccxy shim did not reach claudex. resolved ovm: $(command -v ovm). output: $bare_out"
pass "bare ccxy shim → claudex"

# ---------------------------------------------------------------------------
echo "→ claudex session hook survives the self-managed child marker"
# This hook runs from inside a claudex-launched Claude, which carries the
# marker; it must dispatch to the plugin, not error at the ovm CLI.
hook_out=$(printf '{"session_id":"e2e","source":"startup"}' \
  | OVM_SELF_MANAGED_CHILD=1 ovm claudex __session-start 2>&1) || fail "session hook exited non-zero"
echo "$hook_out" | grep -qiE "$OVM_LEVEL" \
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
echo "$adopt_out" | grep -qiE "$OVM_LEVEL" \
  && fail "ovm adopt fell through to the ovm CLI parser. Got: $adopt_out"
[ -x "$TMP/foreign-codex" ] || fail "adopt deleted the original foreign binary"
ovm ls codex 2>&1 | grep -q "$ADOPT_TAG" || fail "adopted version not listed by ovm ls codex"
ovm which codex 2>&1 | grep -q "$ADOPT_TAG" || fail "adopted codex not resolvable via ovm which"
ovm current codex 2>&1 | grep -q "$ADOPT_TAG" || fail "adopted codex not active via ovm current"
pass "ovm adopt codex (original preserved, listed + usable)"

echo "e2e-command-matrix: ok"
