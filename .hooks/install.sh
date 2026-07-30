#!/bin/sh
# Install OVM git hooks into the local .git/hooks/ directory.
# Run from the repo root: sh .hooks/install.sh

set -e

HOOKS_DIR="$(git rev-parse --show-toplevel)/.hooks"
GIT_HOOKS_DIR="$(git rev-parse --git-dir)/hooks"

for hook in pre-commit pre-push; do
    if [ -f "$HOOKS_DIR/$hook" ]; then
        ln -sf "$HOOKS_DIR/$hook" "$GIT_HOOKS_DIR/$hook"
        chmod +x "$GIT_HOOKS_DIR/$hook"
        echo "Installed $hook hook"
    fi
done

echo "Done. Hooks installed from .hooks/ into .git/hooks/"
