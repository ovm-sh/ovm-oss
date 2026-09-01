#!/usr/bin/env bash
# The installer must leave `ovm` findable by the user's shell.
#
# It used to install correctly and only PRINT "add this to your PATH if
# needed". A real user followed the documented one-liner, got
# `zsh: command not found: ovm`, and reasonably concluded the install had
# failed. A working install the shell cannot see is a failed install.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

# Source only the PATH-wiring helpers, so this stays a unit test of the wiring
# and never downloads or installs a release.
helpers="$TMP_DIR/helpers.sh"
awk '/^# --- PATH wiring/,/^# --- end PATH wiring/' "$ROOT/install.sh" > "$helpers"
if [ ! -s "$helpers" ]; then
  # No end marker in the script; take from the start marker to configure_path's
  # closing brace instead.
  awk '/^# --- PATH wiring/{flag=1} flag{print} flag&&/^}$/&&seen{exit} /^configure_path\(\)/{seen=1}' \
    "$ROOT/install.sh" > "$helpers"
fi
grep -q 'configure_path()' "$helpers" || {
  echo "could not extract PATH helpers from install.sh" >&2
  exit 1
}

run_case() {
  local name=$1 shell_name=$2
  shift 2
  local home="$TMP_DIR/$name"
  mkdir -p "$home/.ovm/bin"
  # configure_path WRITES, print_path_outro SPEAKS. They were one function
  # until the outro had to move to the end of the install: the hatch runs in
  # between, and its first screen clears the terminal, so anything said here
  # was wiped before the reader could act on it. The pair is the unit — what
  # the user ends up seeing is unchanged, and that is what these cases assert.
  # shellcheck disable=SC2016  # deliberately unexpanded in the child shell
  HOME="$home" SHELL="/bin/$shell_name" PATH="/usr/bin:/bin" \
    sh -c ". '$helpers'; configure_path \"\$HOME/.ovm/bin\"; print_path_outro" \
    > "$home/out.txt" 2>&1
  echo "$home"
}

# --- zsh: both interactive and login shells must see it --------------------
home=$(run_case zsh zsh)
[ -f "$home/.zshrc" ] || { echo "zsh: .zshrc was not written" >&2; exit 1; }
# `zsh -lc` sources .zprofile but NOT .zshrc. Writing only .zshrc produces a
# PATH that works when you type in a terminal but not when a script checks —
# exactly the contradiction that made this bug hard to confirm.
[ -f "$home/.zprofile" ] || { echo "zsh: .zprofile was not written" >&2; exit 1; }
# shellcheck disable=SC2016  # the literal string $HOME is what must be in the file
grep -Fq '$HOME/.ovm/bin' "$home/.zshrc" || {
  echo "zsh: rc line does not reference the install dir" >&2; exit 1;
}
grep -Fq 'Added OVM to your PATH' "$home/out.txt" || {
  echo "zsh: the user was not told what changed" >&2; cat "$home/out.txt" >&2; exit 1;
}

# The line must survive a moved home directory, so it is written symbolically.
grep -Fq "$home" "$home/.zshrc" && {
  echo "zsh: rc line hardcodes an absolute home path" >&2; exit 1;
}

# --- rerunning the installer must not stack duplicate blocks ---------------
before=$(grep -c 'ovm' "$home/.zshrc")
HOME="$home" SHELL=/bin/zsh PATH="/usr/bin:/bin" \
  sh -c ". '$helpers'; configure_path \"\$HOME/.ovm/bin\"" >/dev/null 2>&1
after=$(grep -c 'ovm' "$home/.zshrc")
[ "$before" = "$after" ] || {
  echo "rerunning the installer duplicated the PATH block ($before -> $after)" >&2
  exit 1
}

# --- bash ------------------------------------------------------------------
home=$(run_case bash bash)
[ -f "$home/.bashrc" ] || { echo "bash: .bashrc was not written" >&2; exit 1; }
[ -f "$home/.bash_profile" ] || { echo "bash: .bash_profile was not written" >&2; exit 1; }

# --- fish uses its own syntax; an `export` line would be a syntax error -----
home=$(run_case fish fish)
fish_conf="$home/.config/fish/conf.d/ovm.fish"
[ -f "$fish_conf" ] || { echo "fish: conf.d file was not written" >&2; exit 1; }
grep -Fq 'fish_add_path' "$fish_conf" || {
  echo "fish: expected fish_add_path, got:" >&2; cat "$fish_conf" >&2; exit 1;
}
grep -Fq 'export PATH=' "$fish_conf" && {
  echo "fish: POSIX export syntax would not parse in fish" >&2; exit 1;
}

# --- an unknown shell still gets a POSIX fallback --------------------------
home=$(run_case unknown ksh93)
[ -f "$home/.profile" ] || { echo "unknown shell: .profile was not written" >&2; exit 1; }

# --- opting out must be honoured -------------------------------------------
home="$TMP_DIR/optout"
mkdir -p "$home/.ovm/bin"
HOME="$home" SHELL=/bin/zsh OVM_NO_MODIFY_PATH=1 PATH="/usr/bin:/bin" \
  sh -c ". '$helpers'; configure_path \"\$HOME/.ovm/bin\"; print_path_outro" > "$home/out.txt" 2>&1
[ -f "$home/.zshrc" ] && { echo "OVM_NO_MODIFY_PATH was ignored" >&2; exit 1; }
grep -Fq 'OVM_NO_MODIFY_PATH' "$home/out.txt" || {
  echo "opt-out was not explained to the user" >&2; exit 1;
}

# --- already on PATH: say so, change nothing --------------------------------
home="$TMP_DIR/already"
mkdir -p "$home/.ovm/bin"
HOME="$home" SHELL=/bin/zsh PATH="$home/.ovm/bin:/usr/bin:/bin" \
  sh -c ". '$helpers'; configure_path \"\$HOME/.ovm/bin\"; print_path_outro" > "$home/out.txt" 2>&1
[ -f "$home/.zshrc" ] && { echo "rc file written when PATH was already set" >&2; exit 1; }
grep -Fq 'already' "$home/out.txt" || {
  echo "did not report that PATH was already set" >&2; exit 1;
}

# --- an unwritable shell config must NOT read as a clean install ------------
# OVM on disk that the shell cannot find is the exact state users report as a
# failed install, so silence here would recreate the original incident under a
# different filesystem condition.
home="$TMP_DIR/readonly"
mkdir -p "$home/.ovm/bin"
: > "$home/.zshrc"
chmod 0444 "$home/.zshrc"
chmod 0555 "$home"
HOME="$home" SHELL=/bin/zsh PATH="/usr/bin:/bin" \
  sh -c ". '$helpers'; configure_path \"\$HOME/.ovm/bin\"; print_path_outro" > "$TMP_DIR/readonly-out.txt" 2>&1 || true
chmod 0755 "$home"
grep -Fq 'NOT on your PATH' "$TMP_DIR/readonly-out.txt" || {
  echo "an unwritable shell config was reported as a clean install:" >&2
  cat "$TMP_DIR/readonly-out.txt" >&2
  exit 1
}

# --- the outro speaks; configure_path stays silent --------------------------
# This is the whole point of the split. 0.1.7 printed the PATH advice BEFORE
# offering the hatch, and the tour's opening screen-clear wiped it — on the
# default answer, every time — leaving the reader in a shell that could not
# find ovm with the explanation four screens behind them. Assert the property,
# not just the text: nothing may be said until the install is over.
home="$TMP_DIR/silent"
mkdir -p "$home/.ovm/bin"
HOME="$home" SHELL=/bin/zsh PATH="/usr/bin:/bin" \
  sh -c ". '$helpers'; configure_path \"\$HOME/.ovm/bin\"" > "$home/out.txt" 2>&1
if [ -s "$home/out.txt" ]; then
  echo "configure_path spoke before the install finished:" >&2
  cat "$home/out.txt" >&2
  exit 1
fi
[ -f "$home/.zshrc" ] || { echo "configure_path did not write the rc file" >&2; exit 1; }

# --- after the tour, the installer does not repeat it -----------------------
# The tour closes with its own version of this, naming the shortcuts it just
# installed. Two closing screens disagreeing about what to type next is worse
# than either alone.
home="$TMP_DIR/hatched"
mkdir -p "$home/.ovm/bin"
HOME="$home" SHELL=/bin/zsh PATH="/usr/bin:/bin" \
  sh -c ". '$helpers'; configure_path \"\$HOME/.ovm/bin\"; HATCH_RAN=1; print_path_outro" \
  > "$home/out.txt" 2>&1
if [ -s "$home/out.txt" ]; then
  echo "the installer repeated the tour's closing advice:" >&2
  cat "$home/out.txt" >&2
  exit 1
fi

# --- ...unless the rc write failed, where the tour's advice is untrue -------
# The tour says "open a new terminal session". With no rc line written, a new
# terminal will not have it either, so the accurate warning must get the last
# word even though the tour already spoke.
home="$TMP_DIR/hatched-readonly"
mkdir -p "$home/.ovm/bin"
: > "$home/.zshrc"
chmod 0444 "$home/.zshrc"
chmod 0555 "$home"
HOME="$home" SHELL=/bin/zsh PATH="/usr/bin:/bin" \
  sh -c ". '$helpers'; configure_path \"\$HOME/.ovm/bin\"; HATCH_RAN=1; print_path_outro" \
  > "$TMP_DIR/hatched-readonly-out.txt" 2>&1 || true
chmod 0755 "$home"
grep -Fq 'NOT on your PATH' "$TMP_DIR/hatched-readonly-out.txt" || {
  echo "a failed rc write went unmentioned because the tour had run:" >&2
  cat "$TMP_DIR/hatched-readonly-out.txt" >&2
  exit 1
}

echo "install-path-wiring: ok"
