#!/usr/bin/env python3
"""Stamp registry content identity independently of schema compatibility.

Timestamp metadata does not change a snapshot. Release dates and retirement
state remain semantic content. Aggregate rows bind the exact product snapshot.
Canonical encoding is Python JSON (sorted keys, compact separators, UTF-8).
Other serializers must not assume identical digests for numeric values.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path

TIMESTAMP_FIELDS = frozenset({"updated_at", "generated_at", "timestamp"})


def semantic_content(value, *, root: bool = True):
    if isinstance(value, dict):
        return {
            key: semantic_content(item, root=False)
            for key, item in value.items()
            if key not in TIMESTAMP_FIELDS and not (root and key == "snapshot_revision")
        }
    if isinstance(value, list):
        return [semantic_content(item, root=False) for item in value]
    return value


def snapshot_revision(data: dict) -> str:
    canonical = json.dumps(semantic_content(data), sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    return "sha256:" + hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def stamp_document(data: dict) -> dict:
    """Stamp one registry or model document without changing lifecycle dates."""
    stamped = {**data}
    stamped.setdefault("schema_version", 1)
    stamped["snapshot_revision"] = snapshot_revision(stamped)
    return stamped


def stamp_registry(directory: Path) -> None:
    index_path = directory / "registry.json"
    index = json.loads(index_path.read_text())
    index.setdefault("schema_version", 1)
    updates = []
    for row in index.get("products", []):
        product = row["product"]
        if not isinstance(product, str) or not re.fullmatch(r"[a-z][a-z0-9-]*", product):
            raise ValueError("invalid registry product identifier")
        path = directory / f"{product}.json"
        data = stamp_document(json.loads(path.read_text()))
        row["snapshot_revision"] = data["snapshot_revision"]
        updates.append((path, data))
    index["snapshot_revision"] = snapshot_revision(index)
    updates.append((index_path, index))
    # Resolve every input before writing: missing or malformed product data
    # must never leave an apparently complete aggregate identity behind.
    for path, data in updates:
        text = json.dumps(data, indent=2, ensure_ascii=False) + "\n"
        if path.read_text() != text:
            path.write_text(text)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--registry-dir", type=Path, required=True)
    stamp_registry(parser.parse_args().registry_dir)
