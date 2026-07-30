"""Shared Codex SQLite migration classification.

Migration rebuilds must be evaluated against the schema they replace. Regexes
alone cannot tell whether `threads_new` preserved every legacy column before it
was renamed back to `threads`.
"""

from __future__ import annotations

import re
import sqlite3
from dataclasses import dataclass

SCRATCH = re.compile(r"(_migration|_new|_old|_tmp|_temp)$", re.I)


@dataclass(frozen=True)
class MigrationClassification:
    breaking: bool
    removed: tuple[str, ...]
    indeterminate: bool = False


class MigrationClassifier:
    """Apply migrations in order and compare each resulting SQLite schema."""

    def __init__(self) -> None:
        self._connection = sqlite3.connect(":memory:")

    def classify(self, sql: str) -> MigrationClassification:
        before = _schema(self._connection)
        candidate = sqlite3.connect(":memory:")
        self._connection.backup(candidate)
        try:
            candidate.executescript(sql)
            after = _schema(candidate)
        except sqlite3.Error:
            candidate.close()
            return _conservative_classification(sql)

        removed = set(_explicit_removals(sql))
        for table, columns in before.items():
            if SCRATCH.search(table):
                continue
            if table not in after:
                removed.add(table)
                continue
            removed.update(f"{table}.{column}" for column in columns - after[table])

        self._connection.close()
        self._connection = candidate
        ordered = tuple(sorted(removed))
        return MigrationClassification(bool(ordered), ordered)


def _schema(connection: sqlite3.Connection) -> dict[str, set[str]]:
    tables = {
        row[0]
        for row in connection.execute(
            "SELECT name FROM sqlite_schema "
            "WHERE type = 'table' AND name NOT LIKE 'sqlite_%'"
        )
    }
    return {
        table: {
            row[1]
            for row in connection.execute(
                f'PRAGMA table_info("{table.replace(chr(34), chr(34) * 2)}")'
            )
        }
        for table in tables
    }


def _names(pattern: str, sql: str) -> set[str]:
    return {match.lower() for match in re.findall(pattern, sql, re.I)}


def _explicit_removals(sql: str) -> set[str]:
    dropped = _names(
        r'DROP\s+TABLE\s+(?:IF\s+EXISTS\s+)?["`]?([\w]+)', sql
    )
    recreated = _names(
        r'CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?["`]?([\w]+)', sql
    )
    renamed = _names(r'RENAME\s+TO\s+["`]?([\w]+)', sql)
    dropped_columns = _names(r'DROP\s+COLUMN\s+["`]?([\w]+)', sql)
    return {
        item
        for item in (dropped - recreated - renamed) | dropped_columns
        if not SCRATCH.search(item)
    }


def _conservative_classification(sql: str) -> MigrationClassification:
    removed = _explicit_removals(sql)
    dropped = _names(
        r'DROP\s+TABLE\s+(?:IF\s+EXISTS\s+)?["`]?([\w]+)', sql
    )
    renamed = _names(r'RENAME\s+TO\s+["`]?([\w]+)', sql)
    # When the old schema is unavailable or the migration cannot be replayed,
    # a drop-and-rename rebuild is not safe merely because the final table name
    # matches. Preserve it as an explicit indeterminate/destructive signal.
    rebuilds = {
        table
        for table in dropped & renamed
        if not SCRATCH.search(table)
    }
    removed.update(f"{table} (rebuild schema unverified)" for table in rebuilds)
    ordered = tuple(sorted(removed))
    # Reaching this fallback means SQLite could not apply the migration to the
    # schema replayed so far. Even when no DROP pattern is recognizable, the
    # result is unknown rather than definitively additive.
    return MigrationClassification(bool(ordered), ordered, True)
