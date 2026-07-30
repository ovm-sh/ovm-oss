#!/usr/bin/env python3
"""Generate or verify the CODEX_STATE_MIGRATIONS manifest.

Codex stores state in a single forward-migrated SQLite DB. A migration is
"breaking" for cross-version use when it removes a table/column an older binary
still reads: a DROP without a same-name recreate or RENAME TO. Rebuilds and
scratch tables are not breaking.

Usage:
  gen-codex-migration-manifest.py MIGRATIONS_DIR [--source-ref TAG]
  gen-codex-migration-manifest.py MIGRATIONS_DIR --source-ref TAG --check TARGET
  gen-codex-migration-manifest.py MIGRATIONS_DIR --source-ref TAG --write TARGET
"""

import argparse
import difflib
import glob
import os
import re
import sys
from typing import Optional

from codex_schema import MigrationClassifier

BEGIN_MARKER = "// CODEX_STATE_MIGRATIONS_BEGIN"
END_MARKER = "// CODEX_STATE_MIGRATIONS_END"


def escape_rust_text(value: str) -> str:
    """Escape untrusted text for a Rust string or single-line comment."""
    escaped = []
    replacements = {
        "\\": "\\\\",
        '"': '\\"',
        "\n": "\\n",
        "\r": "\\r",
        "\t": "\\t",
    }
    for character in value:
        if character in replacements:
            escaped.append(replacements[character])
        elif ord(character) < 0x20 or ord(character) == 0x7F:
            escaped.append(f"\\u{{{ord(character):x}}}")
        else:
            escaped.append(character)
    return "".join(escaped)


def render_manifest(files: list[str], source_ref: str, source_commit: Optional[str]) -> str:
    classifier = MigrationClassifier()
    lines = [
        "// Codex `state` migrator — generated from openai/codex codex-rs/state/migrations",
        f"// at {escape_rust_text(source_ref)} "
        "(regenerate with scripts/gen-codex-migration-manifest.py).",
    ]
    if source_commit:
        lines.append(f"// source commit: {escape_rust_text(source_commit)}")
    lines.extend(
        [
            "// Keep in version",
            "// order; `breaking` flags removals only.",
            "#[rustfmt::skip]",
            "const CODEX_STATE_MIGRATIONS: &[Migration] = &[",
        ]
    )
    for path in files:
        with open(path, encoding="utf-8") as migration_file:
            sql = migration_file.read()
        base = os.path.basename(path)[:-4]
        match = re.match(r"0*(\d+)", base)
        if not match:
            raise ValueError(f"migration filename has no numeric prefix: {base}.sql")
        version = int(match.group(1))
        description = escape_rust_text(
            re.sub(r"^\d+_", "", base).replace("_", " ")
        )
        classification = classifier.classify(sql)
        if classification.indeterminate:
            raise ValueError(
                f"migration replay failed; refusing a false-safe manifest: {base}.sql"
            )
        breaking = "true" if classification.breaking else "false"
        note = (
            f' // removes {escape_rust_text(", ".join(classification.removed))}'
            if classification.removed
            else ""
        )
        lines.append(
            f'    Migration {{ version: {version}, description: "{description}", '
            f"breaking: {breaking} }},{note}"
        )
    lines.append("];")
    return "\n".join(lines) + "\n"


def replace_manifest(target_text: str, generated: str) -> str:
    if target_text.count(BEGIN_MARKER) != 1 or target_text.count(END_MARKER) != 1:
        raise ValueError("target must contain exactly one migration manifest marker pair")
    before, remainder = target_text.split(BEGIN_MARKER, 1)
    _, after = remainder.split(END_MARKER, 1)
    return f"{before}{BEGIN_MARKER}\n{generated}{END_MARKER}{after}"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("migrations_dir")
    parser.add_argument("--source-ref", default="unversioned upstream source")
    parser.add_argument("--source-commit")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", metavar="TARGET")
    mode.add_argument("--write", metavar="TARGET")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    files = sorted(glob.glob(os.path.join(args.migrations_dir, "*.sql")))
    if not files:
        print(f"no .sql migrations under {args.migrations_dir}", file=sys.stderr)
        return 1

    try:
        generated = render_manifest(files, args.source_ref, args.source_commit)
    except (OSError, ValueError) as error:
        print(f"cannot generate migration manifest: {error}", file=sys.stderr)
        return 1

    target_arg = args.check or args.write
    if not target_arg:
        print(generated, end="")
        return 0

    target = os.path.abspath(target_arg)
    try:
        with open(target, encoding="utf-8") as target_file:
            current = target_file.read()
        updated = replace_manifest(current, generated)
    except (OSError, ValueError) as error:
        print(f"cannot update {target}: {error}", file=sys.stderr)
        return 1

    if args.check:
        if current == updated:
            return 0
        diff = difflib.unified_diff(
            current.splitlines(),
            updated.splitlines(),
            fromfile=target,
            tofile=f"{target} (generated from {args.source_ref})",
            lineterm="",
        )
        print("\n".join(diff), file=sys.stderr)
        return 1

    temporary = f"{target}.tmp.{os.getpid()}"
    try:
        with open(temporary, "w", encoding="utf-8") as output:
            output.write(updated)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, target)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
