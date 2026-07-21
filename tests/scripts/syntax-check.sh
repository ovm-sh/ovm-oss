#!/usr/bin/env bash
# Syntax-check every shell script under scripts/ using the interpreter named
# in its shebang. Catches zsh-isms slipping into bash files (and vice versa)
# before they break a scheduled workflow.
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo_root"

fail=0
for f in scripts/*.sh; do
    shebang=$(head -n 1 "$f")
    case "$shebang" in
        *bash*) checker=(bash -n) ;;
        *zsh*)  checker=(zsh -n) ;;
        *sh)    checker=(sh -n) ;;
        *)
            echo "skip (unknown shebang): $f"
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
