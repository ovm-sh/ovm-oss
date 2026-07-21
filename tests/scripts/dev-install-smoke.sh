#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT
export HOME="$TMP_DIR/home"
export OVM_INSTALL_DIR="$HOME/.ovm/bin"
mkdir -p "$HOME/.cargo/bin" "$OVM_INSTALL_DIR"
LOCK_HELPER="$TMP_DIR/lock-helper.py"
cat > "$LOCK_HELPER" <<'PY'
#!/usr/bin/env python3
import fcntl
import os
import sys

lock_path = os.environ["OVM_SELF_LOCK_HELPER_PATH"]
ready_path = os.environ["OVM_SELF_LOCK_HELPER_READY"]
os.makedirs(os.path.dirname(lock_path), exist_ok=True)
with open(lock_path, "a+", encoding="utf-8") as lock_file:
    fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
    open(ready_path, "w", encoding="utf-8").close()
    sys.stdin.buffer.read()
PY
chmod +x "$LOCK_HELPER"
export OVM_SELF_LOCK_HELPER_BINARY="$LOCK_HELPER"

make_bundle() {
  local name=$1
  shift
  local dir="$TMP_DIR/$name"
  local manifest="$TMP_DIR/$name.tsv"
  mkdir -p "$dir"
  {
    printf 'ovm-bundle-v1\nmain\tovm\tovm\n'
    for binary in "$@"; do
      printf 'side\t%s\t%s\n' "$binary" "$binary"
    done
  } > "$manifest"
  for binary in ovm "$@"; do
    if [[ "$binary" == ovm ]]; then
      cat > "$dir/$binary" <<EOF
#!/bin/sh
if [ "\${1:-}" = self ] && [ "\${2:-}" = current ]; then
  basename "\$(readlink "\$HOME/.ovm/self/current")"
  exit 0
fi
echo '$name:$binary'
EOF
    else
      cat > "$dir/$binary" <<EOF
#!/bin/sh
echo '$name:$binary'
EOF
    fi
    chmod +x "$dir/$binary"
  done
  printf '%s|%s\n' "$dir" "$manifest"
}

PRIVATE_PLUGIN="$TMP_DIR/ovm-diff"
cat > "$PRIVATE_PLUGIN" <<'EOF'
#!/bin/sh
echo private-diff
EOF
chmod +x "$PRIVATE_PLUGIN"

first=$(make_bundle first ovm-alpha)
FIRST_DIR=${first%%|*}
FIRST_MANIFEST=${first#*|}

# A failed first activation leaves no broken control plane or active pointer.
fresh_bad=$(make_bundle fresh-bad)
FRESH_BAD_DIR=${fresh_bad%%|*}
FRESH_BAD_MANIFEST=${fresh_bad#*|}
printf '#!/bin/sh\nexit 42\n' > "$FRESH_BAD_DIR/ovm"
chmod +x "$FRESH_BAD_DIR/ovm"
FRESH_HOME="$TMP_DIR/fresh-home"
if HOME="$FRESH_HOME" \
  OVM_INSTALL_DIR="$FRESH_HOME/.ovm/bin" \
  OVM_LOCAL_ARTIFACT_DIR="$FRESH_BAD_DIR" \
  OVM_LOCAL_MANIFEST="$FRESH_BAD_MANIFEST" \
  OVM_LOCAL_VERSION="fresh-bad" \
  OVM_REFRESH_CONTROL=1 \
  sh "$ROOT/install.sh" >/dev/null 2>&1; then
  echo "failed first control plane unexpectedly activated" >&2
  exit 1
fi
[[ ! -e "$FRESH_HOME/.ovm/bin/ovm" && ! -L "$FRESH_HOME/.ovm/bin/ovm" ]] || { echo "ASSERT FAILED at -e:89" >&2; exit 1; }
[[ ! -e "$FRESH_HOME/.ovm/self/current" && ! -L "$FRESH_HOME/.ovm/self/current" ]] || { echo "ASSERT FAILED at -e:90" >&2; exit 1; }

# An unowned regular control binary is never replaced or adopted implicitly.
FOREIGN_HOME="$TMP_DIR/foreign-home"
mkdir -p "$FOREIGN_HOME/.ovm/bin"
printf 'foreign-control\n' > "$FOREIGN_HOME/.ovm/bin/ovm"
printf 'foreign-skew\n' > "$FOREIGN_HOME/.ovm/bin/ovm-codex-skew"
printf 'foreign-claudex\n' > "$FOREIGN_HOME/.ovm/bin/ovm-claudex"
chmod +x "$FOREIGN_HOME/.ovm/bin/ovm" \
  "$FOREIGN_HOME/.ovm/bin/ovm-codex-skew" \
  "$FOREIGN_HOME/.ovm/bin/ovm-claudex"
if HOME="$FOREIGN_HOME" \
  OVM_INSTALL_DIR="$FOREIGN_HOME/.ovm/bin" \
  OVM_LOCAL_ARTIFACT_DIR="$FIRST_DIR" \
  OVM_LOCAL_MANIFEST="$FIRST_MANIFEST" \
  OVM_LOCAL_VERSION="foreign-blocked" \
  sh "$ROOT/install.sh" >/dev/null 2>&1; then
  echo "foreign control plane was unexpectedly replaced" >&2
  exit 1
fi
[[ $(<"$FOREIGN_HOME/.ovm/bin/ovm") == foreign-control ]] || { echo "ASSERT FAILED at -e:110" >&2; exit 1; }
[[ $(<"$FOREIGN_HOME/.ovm/bin/ovm-codex-skew") == foreign-skew ]] || { echo "ASSERT FAILED at -e:111" >&2; exit 1; }
[[ $(<"$FOREIGN_HOME/.ovm/bin/ovm-claudex") == foreign-claudex ]] || { echo "ASSERT FAILED at -e:112" >&2; exit 1; }
[[ ! -e "$FOREIGN_HOME/.ovm/self/current" && ! -L "$FOREIGN_HOME/.ovm/self/current" ]] || { echo "ASSERT FAILED at -e:113" >&2; exit 1; }

# A control plane left by the retired checkout-symlink workflow migrates only
# when OVM_LEGACY_ROOT authorizes it; a symlink to anywhere else stays foreign.
SYMLINK_HOME="$TMP_DIR/symlink-home"
SYMLINK_CHECKOUT="$TMP_DIR/symlink-checkout"
mkdir -p "$SYMLINK_HOME/.ovm/bin" "$SYMLINK_CHECKOUT/target/release"
cp "$FIRST_DIR/ovm" "$SYMLINK_CHECKOUT/target/release/ovm"
ln -s "$SYMLINK_CHECKOUT/target/release/ovm" "$SYMLINK_HOME/.ovm/bin/ovm"
if HOME="$SYMLINK_HOME" \
  OVM_INSTALL_DIR="$SYMLINK_HOME/.ovm/bin" \
  OVM_LOCAL_ARTIFACT_DIR="$FIRST_DIR" \
  OVM_LOCAL_MANIFEST="$FIRST_MANIFEST" \
  OVM_LOCAL_VERSION="symlink-blocked" \
  sh "$ROOT/install.sh" >/dev/null 2>&1; then
  echo "symlink control plane migrated without legacy authorization" >&2
  exit 1
fi
[[ -L "$SYMLINK_HOME/.ovm/bin/ovm" ]] || { echo "ASSERT FAILED at -e:131" >&2; exit 1; }
[[ ! -e "$SYMLINK_HOME/.ovm/self/current" && ! -L "$SYMLINK_HOME/.ovm/self/current" ]] || { echo "ASSERT FAILED at -e:132" >&2; exit 1; }
HOME="$SYMLINK_HOME" \
  OVM_INSTALL_DIR="$SYMLINK_HOME/.ovm/bin" \
  OVM_LOCAL_ARTIFACT_DIR="$FIRST_DIR" \
  OVM_LOCAL_MANIFEST="$FIRST_MANIFEST" \
  OVM_LOCAL_VERSION="symlink-migrated" \
  OVM_LEGACY_ROOT="$SYMLINK_CHECKOUT" \
  sh "$ROOT/install.sh" >/dev/null
[[ -f "$SYMLINK_HOME/.ovm/bin/ovm" && ! -L "$SYMLINK_HOME/.ovm/bin/ovm" ]] || { echo "ASSERT FAILED at -e:140" >&2; exit 1; }
[[ $(basename "$(readlink "$SYMLINK_HOME/.ovm/self/current")") == symlink-migrated ]] || { echo "ASSERT FAILED at -e:141" >&2; exit 1; }

# An explicitly authorized marker-less bundle from the retired installer is migrated.
legacy=$(make_bundle legacy-direct ovm-codex-skew ovm-claudex)
LEGACY_DIR=${legacy%%|*}
LEGACY_MANIFEST=${legacy#*|}
LEGACY_HOME="$TMP_DIR/legacy-home"
mkdir -p "$LEGACY_HOME/.ovm/bin"
printf '#!/bin/sh\necho old-ovm\n' > "$LEGACY_HOME/.ovm/bin/ovm"
printf '#!/bin/sh\necho old-skew\n' > "$LEGACY_HOME/.ovm/bin/ovm-codex-skew"
printf '#!/bin/sh\necho old-claudex\n' > "$LEGACY_HOME/.ovm/bin/ovm-claudex"
chmod +x "$LEGACY_HOME/.ovm/bin/ovm" \
  "$LEGACY_HOME/.ovm/bin/ovm-codex-skew" \
  "$LEGACY_HOME/.ovm/bin/ovm-claudex"
HOME="$LEGACY_HOME" \
OVM_INSTALL_DIR="$LEGACY_HOME/.ovm/bin" \
OVM_LOCAL_ARTIFACT_DIR="$LEGACY_DIR" \
OVM_LOCAL_MANIFEST="$LEGACY_MANIFEST" \
OVM_LOCAL_VERSION="legacy-direct" \
OVM_MIGRATE_LEGACY_DIRECT=1 \
sh "$ROOT/install.sh" >/dev/null
[[ -f "$LEGACY_HOME/.ovm/bin/ovm" && ! -L "$LEGACY_HOME/.ovm/bin/ovm" ]] || { echo "ASSERT FAILED at -e:162" >&2; exit 1; }
[[ $(readlink "$LEGACY_HOME/.ovm/bin/ovm-codex-skew") == ovm ]] || { echo "ASSERT FAILED at -e:163" >&2; exit 1; }
[[ $(readlink "$LEGACY_HOME/.ovm/bin/ovm-claudex") == ovm ]] || { echo "ASSERT FAILED at -e:164" >&2; exit 1; }
[[ -f "$LEGACY_HOME/.ovm/self/launcher-dir" ]] || { echo "ASSERT FAILED at -e:165" >&2; exit 1; }
[[ $(basename "$(readlink "$LEGACY_HOME/.ovm/self/current")") == legacy-direct ]] || { echo "ASSERT FAILED at -e:166" >&2; exit 1; }

OLD_ROOT="$TMP_DIR/moved-away/ovm"
ln -s "$OLD_ROOT/target/release/ovm" "$HOME/.cargo/bin/ovm"
ln -s "$OLD_ROOT/target/release/ovm-claudex" "$HOME/.cargo/bin/ovm-claudex"
ln -s "$OLD_ROOT/target/release/ovm-alpha" "$OVM_INSTALL_DIR/ovm-alpha"
for product in claude codex pi; do
  ln -s "$OLD_ROOT/target/release/ovm" "$OVM_INSTALL_DIR/$product"
done
ln -s "$OLD_ROOT/plugins/diff/ovm-diff" "$OVM_INSTALL_DIR/ovm-diff"
printf 'foreign\n' > "$OVM_INSTALL_DIR/ovm-private"

OVM_DEV_SKIP_BUILD=1 \
OVM_DEV_ARTIFACT_DIR="$FIRST_DIR" \
OVM_BUNDLE_MANIFEST="$FIRST_MANIFEST" \
OVM_DEV_PRIVATE_PLUGIN="$PRIVATE_PLUGIN" \
OVM_LEGACY_ROOT="$OLD_ROOT" \
sh "$ROOT/scripts/dev-install.sh"

[[ -f "$OVM_INSTALL_DIR/ovm" && ! -L "$OVM_INSTALL_DIR/ovm" && -x "$OVM_INSTALL_DIR/ovm" ]] || { echo "ASSERT FAILED at -e:185" >&2; exit 1; }
[[ -L "$HOME/.ovm/self/current" ]] || { echo "ASSERT FAILED at -e:186" >&2; exit 1; }
first_current=$(readlink "$HOME/.ovm/self/current")
[[ -f "$first_current/.complete" ]] || { echo "ASSERT FAILED at -e:188" >&2; exit 1; }
[[ -f "$first_current/ovm-bundle-v1.tsv" ]] || { echo "ASSERT FAILED at -e:189" >&2; exit 1; }
[[ $(readlink "$OVM_INSTALL_DIR/ovm-alpha") == ovm ]] || { echo "ASSERT FAILED at -e:190" >&2; exit 1; }
# install.sh repoints managed product launchers at the control plane by
# absolute path; dev-install's own migration writes the relative form.
# Either target means "managed, routed through the control plane".
INSTALL_DIR_REAL=$(cd "$OVM_INSTALL_DIR" && pwd -P)
for product in claude codex pi; do
  launcher_target=$(readlink "$OVM_INSTALL_DIR/$product")
  [[ "$launcher_target" == ovm || "$launcher_target" == "$INSTALL_DIR_REAL/ovm" ]] || { echo "ASSERT FAILED: $product -> $launcher_target" >&2; exit 1; }
done
[[ ! -e "$HOME/.cargo/bin/ovm" && ! -L "$HOME/.cargo/bin/ovm" ]] || { echo "ASSERT FAILED at -e:194" >&2; exit 1; }
[[ ! -e "$HOME/.cargo/bin/ovm-claudex" && ! -L "$HOME/.cargo/bin/ovm-claudex" ]] || { echo "ASSERT FAILED at -e:195" >&2; exit 1; }
[[ -f "$OVM_INSTALL_DIR/ovm-diff" && ! -L "$OVM_INSTALL_DIR/ovm-diff" ]] || { echo "ASSERT FAILED at -e:196" >&2; exit 1; }
[[ $(<"$OVM_INSTALL_DIR/ovm-private") == foreign ]] || { echo "ASSERT FAILED at -e:197" >&2; exit 1; }
cp "$OVM_INSTALL_DIR/ovm" "$TMP_DIR/control-first"

# A crashing replacement control plane is probed and automatically restored.
bad=$(make_bundle bad-control)
BAD_DIR=${bad%%|*}
BAD_MANIFEST=${bad#*|}
printf '#!/bin/sh\nexit 42\n' > "$BAD_DIR/ovm"
chmod +x "$BAD_DIR/ovm"
if OVM_LOCAL_ARTIFACT_DIR="$BAD_DIR" \
  OVM_LOCAL_MANIFEST="$BAD_MANIFEST" \
  OVM_LOCAL_VERSION="bad-control" \
  OVM_REFRESH_CONTROL=1 \
  OVM_INSTALL_DIR="$OVM_INSTALL_DIR" \
  sh "$ROOT/install.sh" >/dev/null 2>&1; then
  echo "crashing control plane unexpectedly activated" >&2
  exit 1
fi
[[ $(readlink "$HOME/.ovm/self/current") == "$first_current" ]] || { echo "ASSERT FAILED at -e:215" >&2; exit 1; }
[[ ! -e "$HOME/.ovm/self/previous" && ! -L "$HOME/.ovm/self/previous" ]] || { echo "ASSERT FAILED at -e:216" >&2; exit 1; }
[[ ! -e "$HOME/.ovm/self/control-previous" ]] || { echo "ASSERT FAILED at -e:217" >&2; exit 1; }
cmp "$TMP_DIR/control-first" "$OVM_INSTALL_DIR/ovm"

# A hanging replacement control plane times out and restores the previous state.
hanging=$(make_bundle hanging-control)
HANGING_DIR=${hanging%%|*}
HANGING_MANIFEST=${hanging#*|}
cat > "$HANGING_DIR/ovm" <<'EOF'
#!/bin/sh
if [ "${1:-}" = self ] && [ "${2:-}" = current ]; then
  exec sleep 30
fi
exit 0
EOF
chmod +x "$HANGING_DIR/ovm"
if OVM_LOCAL_ARTIFACT_DIR="$HANGING_DIR" \
  OVM_LOCAL_MANIFEST="$HANGING_MANIFEST" \
  OVM_LOCAL_VERSION="hanging-control" \
  OVM_REFRESH_CONTROL=1 \
  OVM_SELF_UPDATE_PROBE_ATTEMPTS=2 \
  OVM_INSTALL_DIR="$OVM_INSTALL_DIR" \
  sh "$ROOT/install.sh" >/dev/null 2>&1; then
  echo "hanging control plane unexpectedly activated" >&2
  exit 1
fi
[[ $(readlink "$HOME/.ovm/self/current") == "$first_current" ]] || { echo "ASSERT FAILED at -e:242" >&2; exit 1; }
[[ ! -e "$HOME/.ovm/self/previous" && ! -L "$HOME/.ovm/self/previous" ]] || { echo "ASSERT FAILED at -e:243" >&2; exit 1; }
cmp "$TMP_DIR/control-first" "$OVM_INSTALL_DIR/ovm"

# Any failure after publishing current restores the complete prior selection.
injected=$(make_bundle injected ovm-injected)
INJECTED_DIR=${injected%%|*}
INJECTED_MANIFEST=${injected#*|}
if OVM_LOCAL_ARTIFACT_DIR="$INJECTED_DIR" \
  OVM_LOCAL_MANIFEST="$INJECTED_MANIFEST" \
  OVM_LOCAL_VERSION="injected" \
  OVM_REFRESH_CONTROL=0 \
  OVM_TEST_FAIL_AFTER_CURRENT=1 \
  OVM_INSTALL_DIR="$OVM_INSTALL_DIR" \
  sh "$ROOT/install.sh" >/dev/null 2>&1; then
  echo "injected mid-activation failure unexpectedly succeeded" >&2
  exit 1
fi
[[ $(readlink "$HOME/.ovm/self/current") == "$first_current" ]] || { echo "ASSERT FAILED at -e:260" >&2; exit 1; }
[[ ! -e "$HOME/.ovm/self/previous" && ! -L "$HOME/.ovm/self/previous" ]] || { echo "ASSERT FAILED at -e:261" >&2; exit 1; }
[[ $(readlink "$OVM_INSTALL_DIR/ovm-alpha") == ovm ]] || { echo "ASSERT FAILED at -e:262" >&2; exit 1; }
[[ ! -e "$OVM_INSTALL_DIR/ovm-injected" && ! -L "$OVM_INSTALL_DIR/ovm-injected" ]] || { echo "ASSERT FAILED at -e:263" >&2; exit 1; }
[[ $(<"$HOME/.ovm/self/side-links") == ovm-alpha ]] || { echo "ASSERT FAILED at -e:264" >&2; exit 1; }
cmp "$TMP_DIR/control-first" "$OVM_INSTALL_DIR/ovm"

# Reinstalling identical content reuses the immutable version.
OVM_DEV_SKIP_BUILD=1 \
OVM_DEV_ARTIFACT_DIR="$FIRST_DIR" \
OVM_BUNDLE_MANIFEST="$FIRST_MANIFEST" \
OVM_DEV_PRIVATE_PLUGIN="$PRIVATE_PLUGIN" \
OVM_LEGACY_ROOT="$OLD_ROOT" \
sh "$ROOT/scripts/dev-install.sh" >/dev/null
[[ $(readlink "$HOME/.ovm/self/current") == "$first_current" ]] || { echo "ASSERT FAILED at -e:274" >&2; exit 1; }

# Shell and Rust processes serialize on the same advisory lock file.
HOLDER_FIFO="$TMP_DIR/holder.fifo"
HOLDER_READY="$TMP_DIR/holder.ready"
mkfifo "$HOLDER_FIFO"
OVM_SELF_LOCK_HELPER_PATH="$HOME/.ovm/self/.operation.lock" \
OVM_SELF_LOCK_HELPER_READY="$HOLDER_READY" \
"$LOCK_HELPER" < "$HOLDER_FIFO" &
holder_pid=$!
exec 8> "$HOLDER_FIFO"
while [[ ! -f "$HOLDER_READY" ]]; do sleep 0.05; done
# Close fd 8 in the waiter: it must not inherit the FIFO write end, or the
# holder never sees EOF after `exec 8>&-` and the lock is never released.
OVM_DEV_SKIP_BUILD=1 \
OVM_DEV_ARTIFACT_DIR="$FIRST_DIR" \
OVM_BUNDLE_MANIFEST="$FIRST_MANIFEST" \
OVM_DEV_PRIVATE_PLUGIN="$PRIVATE_PLUGIN" \
OVM_LEGACY_ROOT="$OLD_ROOT" \
sh "$ROOT/scripts/dev-install.sh" >/dev/null 2>&1 8>&- &
lock_waiter=$!
sleep 0.2
kill -0 "$lock_waiter"
exec 8>&-
wait "$holder_pid"
wait "$lock_waiter"
rm -f "$HOLDER_FIFO" "$HOLDER_READY"
[[ $(readlink "$HOME/.ovm/self/current") == "$first_current" ]] || { echo "ASSERT FAILED at -e:301" >&2; exit 1; }

# The kernel releases the advisory lock when its holder is killed.
CRASH_FIFO="$TMP_DIR/crash-holder.fifo"
CRASH_READY="$TMP_DIR/crash-holder.ready"
mkfifo "$CRASH_FIFO"
OVM_SELF_LOCK_HELPER_PATH="$HOME/.ovm/self/.operation.lock" \
OVM_SELF_LOCK_HELPER_READY="$CRASH_READY" \
"$LOCK_HELPER" < "$CRASH_FIFO" &
crash_holder=$!
exec 8> "$CRASH_FIFO"
while [[ ! -f "$CRASH_READY" ]]; do sleep 0.05; done
kill -9 "$crash_holder"
wait "$crash_holder" 2>/dev/null || true
exec 8>&-
OVM_DEV_SKIP_BUILD=1 \
OVM_DEV_ARTIFACT_DIR="$FIRST_DIR" \
OVM_BUNDLE_MANIFEST="$FIRST_MANIFEST" \
OVM_DEV_PRIVATE_PLUGIN="$PRIVATE_PLUGIN" \
OVM_LEGACY_ROOT="$OLD_ROOT" \
sh "$ROOT/scripts/dev-install.sh" >/dev/null
rm -f "$CRASH_FIFO" "$CRASH_READY"
[[ $(readlink "$HOME/.ovm/self/current") == "$first_current" ]] || { echo "ASSERT FAILED at -e:323" >&2; exit 1; }

second=$(make_bundle second ovm-beta ovm-gamma)
SECOND_DIR=${second%%|*}
SECOND_MANIFEST=${second#*|}
rm "$OVM_INSTALL_DIR/codex"
ln -s "$first_current/ovm" "$OVM_INSTALL_DIR/codex"
rm "$OVM_INSTALL_DIR/claude"
printf 'foreign-claude\n' > "$OVM_INSTALL_DIR/claude"
chmod 111 "$OVM_INSTALL_DIR/claude"
rm -f "$HOME/.ovm/self/previous"
mkdir "$HOME/.ovm/self/previous"
if OVM_DEV_SKIP_BUILD=1 \
  OVM_DEV_ARTIFACT_DIR="$SECOND_DIR" \
  OVM_BUNDLE_MANIFEST="$SECOND_MANIFEST" \
  OVM_DEV_PRIVATE_PLUGIN="$PRIVATE_PLUGIN" \
  sh "$ROOT/scripts/dev-install.sh" >/dev/null 2>&1; then
  echo "directory at previous pointer unexpectedly accepted" >&2
  exit 1
fi
[[ $(readlink "$HOME/.ovm/self/current") == "$first_current" ]] || { echo "ASSERT FAILED at -e:343" >&2; exit 1; }
[[ ! -e "$OVM_INSTALL_DIR/ovm-beta" && ! -L "$OVM_INSTALL_DIR/ovm-beta" ]] || { echo "ASSERT FAILED at -e:344" >&2; exit 1; }
rmdir "$HOME/.ovm/self/previous"
OVM_DEV_SKIP_BUILD=1 \
OVM_DEV_ARTIFACT_DIR="$SECOND_DIR" \
OVM_BUNDLE_MANIFEST="$SECOND_MANIFEST" \
OVM_DEV_PRIVATE_PLUGIN="$PRIVATE_PLUGIN" \
sh "$ROOT/scripts/dev-install.sh" >/dev/null

second_current=$(readlink "$HOME/.ovm/self/current")
[[ "$second_current" != "$first_current" ]] || { echo "ASSERT FAILED at -e:353" >&2; exit 1; }
[[ $(readlink "$HOME/.ovm/self/previous") == "$first_current" ]] || { echo "ASSERT FAILED at -e:354" >&2; exit 1; }
[[ ! -e "$OVM_INSTALL_DIR/ovm-alpha" && ! -L "$OVM_INSTALL_DIR/ovm-alpha" ]] || { echo "ASSERT FAILED at -e:355" >&2; exit 1; }
[[ $(readlink "$OVM_INSTALL_DIR/ovm-beta") == ovm ]] || { echo "ASSERT FAILED at -e:356" >&2; exit 1; }
[[ $(readlink "$OVM_INSTALL_DIR/ovm-gamma") == ovm ]] || { echo "ASSERT FAILED at -e:357" >&2; exit 1; }
INSTALL_DIR_REAL=$(cd "$OVM_INSTALL_DIR" && pwd -P)
[[ $(readlink "$OVM_INSTALL_DIR/codex") == "$INSTALL_DIR_REAL/ovm" ]] || { echo "ASSERT FAILED at -e:359" >&2; exit 1; }
[[ -f "$OVM_INSTALL_DIR/claude" && ! -L "$OVM_INSTALL_DIR/claude" ]] || { echo "ASSERT FAILED at -e:360" >&2; exit 1; }
cmp "$TMP_DIR/control-first" "$OVM_INSTALL_DIR/ovm"
[[ $(<"$OVM_INSTALL_DIR/ovm-private") == foreign ]] || { echo "ASSERT FAILED at -e:362" >&2; exit 1; }

# A foreign path for a future side binary blocks activation without changing current.
third=$(make_bundle third ovm-beta ovm-gamma ovm-delta)
THIRD_DIR=${third%%|*}
THIRD_MANIFEST=${third#*|}
printf 'foreign-delta\n' > "$OVM_INSTALL_DIR/ovm-delta"
if OVM_DEV_SKIP_BUILD=1 \
  OVM_DEV_ARTIFACT_DIR="$THIRD_DIR" \
  OVM_BUNDLE_MANIFEST="$THIRD_MANIFEST" \
  OVM_DEV_PRIVATE_PLUGIN="$PRIVATE_PLUGIN" \
  sh "$ROOT/scripts/dev-install.sh" >/dev/null 2>&1; then
  echo "foreign side binary was unexpectedly replaced" >&2
  exit 1
fi
[[ $(readlink "$HOME/.ovm/self/current") == "$second_current" ]] || { echo "ASSERT FAILED at -e:377" >&2; exit 1; }
[[ $(<"$OVM_INSTALL_DIR/ovm-delta") == foreign-delta ]] || { echo "ASSERT FAILED at -e:378" >&2; exit 1; }

# A corrupt ownership record falls back to the valid outgoing manifest and is repaired.
printf 'ovm-beta\novm-beta\n' > "$HOME/.ovm/self/side-links"
repair=$(make_bundle repair ovm-beta ovm-gamma)
REPAIR_DIR=${repair%%|*}
REPAIR_MANIFEST=${repair#*|}
OVM_DEV_SKIP_BUILD=1 \
OVM_DEV_ARTIFACT_DIR="$REPAIR_DIR" \
OVM_BUNDLE_MANIFEST="$REPAIR_MANIFEST" \
OVM_DEV_PRIVATE_PLUGIN="$PRIVATE_PLUGIN" \
sh "$ROOT/scripts/dev-install.sh" >/dev/null
second_current=$(readlink "$HOME/.ovm/self/current")
[[ $(<"$HOME/.ovm/self/side-links") == $'ovm-beta\novm-gamma' ]] || { echo "ASSERT FAILED at -e:391" >&2; exit 1; }

# A corrupt outgoing manifest cannot escape the launcher directory during cleanup.
printf 'victim\n' > "$HOME/.ovm/victim"
printf 'ovm-bundle-v1\nmain\tovm\tovm\nside\t../../victim\t-\n' > "$second_current/ovm-bundle-v1.tsv"

# A main-only bundle is valid and removes only links owned by the persisted record.
fourth=$(make_bundle fourth)
FOURTH_DIR=${fourth%%|*}
FOURTH_MANIFEST=${fourth#*|}
OVM_DEV_SKIP_BUILD=1 \
OVM_DEV_ARTIFACT_DIR="$FOURTH_DIR" \
OVM_BUNDLE_MANIFEST="$FOURTH_MANIFEST" \
OVM_DEV_PRIVATE_PLUGIN="$PRIVATE_PLUGIN" \
sh "$ROOT/scripts/dev-install.sh" >/dev/null
[[ ! -e "$OVM_INSTALL_DIR/ovm-beta" && ! -L "$OVM_INSTALL_DIR/ovm-beta" ]] || { echo "ASSERT FAILED at -e:406" >&2; exit 1; }
[[ ! -e "$OVM_INSTALL_DIR/ovm-gamma" && ! -L "$OVM_INSTALL_DIR/ovm-gamma" ]] || { echo "ASSERT FAILED at -e:407" >&2; exit 1; }
[[ $(<"$OVM_INSTALL_DIR/ovm-delta") == foreign-delta ]] || { echo "ASSERT FAILED at -e:408" >&2; exit 1; }
[[ $(<"$HOME/.ovm/victim") == victim ]] || { echo "ASSERT FAILED at -e:409" >&2; exit 1; }
cmp "$TMP_DIR/control-first" "$OVM_INSTALL_DIR/ovm"

# The retired uninstall script never removes standalone self-managed state.
sh "$ROOT/scripts/dev-uninstall.sh" >/dev/null
[[ -f "$OVM_INSTALL_DIR/ovm" ]] || { echo "ASSERT FAILED at -e:414" >&2; exit 1; }
[[ -L "$HOME/.ovm/self/current" ]] || { echo "ASSERT FAILED at -e:415" >&2; exit 1; }

echo "dev-install-smoke: ok"
