#!/bin/sh
# Install OVM git hooks. Run from the repo root: sh .hooks/install.sh

set -e

# core.hooksPath, not symlinks into .git/hooks.
#
# The old installer wrote absolute symlinks. Git silently skips a hook it
# cannot execute, so when this repository moved the hooks kept "existing" while
# doing nothing at all — discovered 2026-09-14, with both links still pointing
# into a directory two moves stale and every local gate quietly off. A relative
# hooksPath survives a move, and `git config --get` below makes the state
# checkable rather than something you have to infer from a dangling link.
if git config core.hooksPath .hooks 2>/dev/null; then
    echo "Hooks enabled via core.hooksPath = .hooks"
    # A stale link here would be inert now, but leaving it invites the next
    # person to "fix" the wrong mechanism.
    for hook in pre-commit pre-push; do
        link="$(git rev-parse --git-dir)/hooks/$hook"
        if [ -L "$link" ]; then
            rm -f "$link"
            echo "  removed the old .git/hooks/$hook symlink"
        fi
    done
else
    # git < 2.9 has no core.hooksPath.
    HOOKS_DIR="$(git rev-parse --show-toplevel)/.hooks"
    GIT_HOOKS_DIR="$(git rev-parse --git-dir)/hooks"
    for hook in pre-commit pre-push; do
        if [ -f "$HOOKS_DIR/$hook" ]; then
            ln -sf "$HOOKS_DIR/$hook" "$GIT_HOOKS_DIR/$hook"
            chmod +x "$GIT_HOOKS_DIR/$hook"
            echo "Installed $hook hook (symlink fallback)"
        fi
    done
fi

echo "Verify with: git config --get core.hooksPath"
