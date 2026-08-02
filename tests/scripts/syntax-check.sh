#!/usr/bin/env bash
# Syntax-check every shell script in the repo using the interpreter named in
# its shebang. Catches zsh-isms slipping into bash files (and vice versa)
# before they break a scheduled workflow.
#
# This used to glob only `scripts/*.sh`, which silently excluded install.sh
# (the file every new user pipes into sh), the git hooks, this suite's own
# tests, the mini-runner scripts and the benchmark scripts — the check reported
# success while never looking at them. Discovery is now shebang-driven over the
# directories that hold scripts, so a new script is covered the moment it lands
# rather than when someone remembers to extend a list.
#
# Discovery deliberately uses globs and `head` only: this script also runs
# inside the exported public tree with a minimal PATH (bash, dirname, head, sh)
# and no git metadata, so `git ls-files` and `find` are not available.
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo_root"

shopt -s nullglob

# Basename of the interpreter a file's shebang selects ("bash", "zsh", "sh",
# "python3", …), resolving `#!/usr/bin/env foo`. Empty when there is no
# shebang. Pure parameter expansion: `basename` is not on the minimal PATH
# this script must survive.
interpreter_of() {
  local line rest name
  line=$(head -n 1 "$1" 2>/dev/null || true)
  case "$line" in
    '#!'*) ;;
    *) return 0 ;;
  esac
  rest=${line#\#!}
  # shellcheck disable=SC2086  # splitting the shebang into words is the point
  set -- $rest
  [ "$#" -gt 0 ] || return 0
  name=${1##*/}
  if [ "$name" = "env" ]; then
    shift
    [ "$#" -gt 0 ] || return 0
    name=${1##*/}
  fi
  printf '%s' "$name"
}

candidates=(
  install.sh
  site/install
  .hooks/*
  scripts/*
  scripts/*/*
  tests/scripts/*
  tools/benchmark/*/*
  docker/*
  npm/*
)

files=()
for candidate in "${candidates[@]}"; do
  [ -f "$candidate" ] || continue
  interp=$(interpreter_of "$candidate")
  case "$interp" in
    # Not shell, and not this check's business.
    ''|python*|node|deno|bun|ruby|perl|php|Rscript|osascript|make|jq) continue ;;
  esac
  files+=("$candidate")
done

# A discovery bug that finds nothing would otherwise pass as "all clear".
[ "${#files[@]}" -gt 0 ] || { echo "discovered no shell scripts to check" >&2; exit 1; }
for required in install.sh tests/scripts/syntax-check.sh; do
  found=0
  for f in "${files[@]}"; do
    [ "$f" = "$required" ] && found=1
  done
  [ "$found" = "1" ] || {
    echo "script discovery missed $required — the glob list is wrong" >&2
    exit 1
  }
done

# `--list` publishes the discovered set so other checks (shellcheck in CI)
# cover exactly the same files. A second hand-maintained list is how five
# scripts went unlinted for months.
if [ "${1:-}" = "--list" ]; then
    printf '%s\n' "${files[@]}"
    exit 0
fi

fail=0
for f in "${files[@]}"; do
    interp=$(interpreter_of "$f")
    case "$interp" in
        bash) checker=(bash -n) ;;
        zsh)
            if ! command -v zsh >/dev/null 2>&1; then
                echo "zsh missing: cannot syntax-check $f" >&2
                fail=1
                continue
            fi
            checker=(zsh -n)
            ;;
        sh|dash|ksh) checker=(sh -n) ;;
        *)
            # Never "skip" quietly: an unrecognized interpreter means this file
            # is going unchecked, and the run still reported success. That is
            # the bug this arm used to have — it printed "skip" and left `fail`
            # untouched.
            echo "unknown interpreter '$interp', cannot syntax-check $f" >&2
            fail=1
            continue
            ;;
    esac
    if ! "${checker[@]}" "$f"; then
        echo "syntax error in $f" >&2
        fail=1
    else
        echo "ok ${checker[0]}: $f"
    fi
done

exit "$fail"
