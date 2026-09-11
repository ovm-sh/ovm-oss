#!/usr/bin/env python3
"""Bind an OVM release to its exact source, archives and optional media.

This records identity, not approval. Public build attestations and acceptance
gates remain the proof. Qualification and public builds have distinct roles.
"""

import argparse
import hashlib
import json
import re
import sys
import tarfile
from pathlib import Path, PurePosixPath


TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
)
TAG = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?")
COMMIT = re.compile(r"[0-9a-f]{40}")


def digest(data):
    return hashlib.sha256(data).hexdigest()


def file_digest(path):
    with path.open("rb") as stream:
        result = hashlib.sha256()
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def revision(document):
    content = {k: v for k, v in document.items() if k != "revision"}
    return "sha256:" + digest(json.dumps(content, sort_keys=True, separators=(",", ":")).encode())


def unique_file(root, name):
    matches = sorted(root.rglob(name))
    if len(matches) != 1 or not matches[0].is_file() or matches[0].is_symlink():
        raise ValueError(f"expected exactly one regular artifact named {name}")
    return matches[0]


def bundle_entries(source_root):
    contents = (source_root / "crates/ovm/ovm-bundle-v1.tsv").read_bytes()
    lines = contents.decode().splitlines()
    if not lines or lines[0] != "ovm-bundle-v1":
        raise ValueError("unsupported bundle manifest")
    entries = []
    for line in lines[1:]:
        role, binary, package = line.split("\t")
        if role not in ("main", "side") or not re.fullmatch(r"ovm(?:-[a-z0-9]+)*", binary):
            raise ValueError("invalid bundle manifest entry")
        entries.append({"role": role, "binary": binary, "package": package})
    if len({entry["binary"] for entry in entries}) != len(entries):
        raise ValueError("duplicate bundled binary")
    if [entry for entry in entries if entry["role"] == "main"] != [
        {"role": "main", "binary": "ovm", "package": "ovm"}
    ]:
        raise ValueError("bundle must have exactly one ovm main binary")
    return contents, entries


def archive_record(root, target, expected_manifest, entries):
    name = f"ovm-{target}.tar.gz"
    archive = unique_file(root, name)
    checksum = unique_file(root, name + ".sha256").read_text().split()
    actual = file_digest(archive)
    if checksum != [actual, name]:
        raise ValueError(f"checksum sidecar does not match {name}")
    binaries = []
    with tarfile.open(archive, "r:gz") as bundle:
        members = {}
        for member in bundle.getmembers():
            path = PurePosixPath(member.name)
            if path.is_absolute() or ".." in path.parts:
                raise ValueError(f"unsafe archive member in {name}")
            if member.isdir():
                continue
            normalized = str(path)
            if not member.isfile() or normalized in members:
                raise ValueError(f"non-regular or duplicate archive member in {name}")
            members[normalized] = member
        expected_names = {"ovm-bundle-v1.tsv", *(entry["binary"] for entry in entries)}
        if set(members) != expected_names:
            raise ValueError(f"{name} differs from the declared bundle contents")
        if bundle.extractfile(members["ovm-bundle-v1.tsv"]).read() != expected_manifest:
            raise ValueError(f"{name} carries a different bundle manifest")
        for entry in entries:
            member = members[entry["binary"]]
            if not member.mode & 0o111:
                raise ValueError(f"{name}: {entry['binary']} is not executable")
            stream = bundle.extractfile(member)
            result = hashlib.sha256()
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                result.update(chunk)
            binaries.append({**entry, "sha256": result.hexdigest()})
    return {"name": name, "target": target, "sha256": actual,
            "size": archive.stat().st_size, "binaries": binaries}


def media_record(path, root, tag, commit, archives, installer_sha256):
    try:
        manifest_path = path.resolve().relative_to(root.resolve()).as_posix()
    except ValueError as error:
        raise ValueError("media manifest must be inside the media root") from error
    media = json.loads(path.read_text())
    if media.get("schema_version") != 1 or media.get("kind") != "ovm-media":
        raise ValueError("unsupported media manifest")
    candidate = media.get("candidate", {})
    if media.get("release_tag") != tag or candidate.get("tag") != tag:
        raise ValueError("media belongs to a different release")
    if candidate.get("source_commit") != commit or candidate.get("installer_sha256") != installer_sha256:
        raise ValueError("media source or installer differs from the release")
    if not any(a["target"] == candidate.get("target") and
               a["sha256"] == candidate.get("archive_sha256") for a in archives):
        raise ValueError("media did not use this release's archive")
    result = {"manifest": manifest_path, "sha256": file_digest(path), "candidate": {
        key: candidate[key] for key in ("tag", "source_commit", "target", "archive_sha256", "installer_sha256")
    }}
    for key in ("output", "poster"):
        asset = media[key]
        relative = PurePosixPath(asset["path"])
        if relative.is_absolute() or ".." in relative.parts:
            raise ValueError("media asset paths must be relative to the media root")
        actual = root / relative
        if not actual.resolve().is_relative_to(root.resolve()):
            raise ValueError("media asset escapes the media root")
        if file_digest(actual) != asset["sha256"]:
            raise ValueError(f"media {key} changed after recording")
        result[key] = {"path": str(relative), "sha256": asset["sha256"]}
    return result


def create(tag, commit, role, artifacts, source_root, media_paths=(), media_root=None):
    if not TAG.fullmatch(tag) or not COMMIT.fullmatch(commit):
        raise ValueError("expected an exact v-prefixed release tag and full source commit")
    if role not in ("public", "qualification"):
        raise ValueError("invalid build role")
    contents, entries = bundle_entries(source_root)
    archives = [archive_record(artifacts, target, contents, entries) for target in TARGETS]
    installer = file_digest(source_root / "install.sh")
    if media_paths and role != "public":
        raise ValueError("shipping media must refer to public artifacts")
    media = [media_record(path, media_root or source_root, tag, commit, archives, installer)
             for path in media_paths]
    document = {
        "schema_version": 1,
        "kind": "ovm-release",
        "release_tag": tag,
        "version": tag[1:],
        "source": {"role": role, "commit": commit},
        "installer_sha256": installer,
        "bundle_manifest_sha256": digest(contents),
        "artifacts": archives,
        "media": media,
    }
    document["revision"] = revision(document)
    return document


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--role", choices=("public", "qualification"), default="public")
    parser.add_argument("--artifacts-dir", type=Path, required=True)
    parser.add_argument("--source-root", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--media-manifest", type=Path, action="append", default=[])
    parser.add_argument("--media-root", type=Path)
    output = parser.add_mutually_exclusive_group(required=True)
    output.add_argument("--output", type=Path)
    output.add_argument("--verify", type=Path)
    args = parser.parse_args()
    try:
        document = create(args.tag, args.source_commit, args.role, args.artifacts_dir,
                          args.source_root, args.media_manifest, args.media_root)
        if args.verify:
            if json.loads(args.verify.read_text()) != document:
                raise ValueError("release manifest differs from the source, artifacts or media")
        else:
            args.output.write_text(json.dumps(document, indent=2) + "\n")
    except (OSError, ValueError, KeyError, TypeError, tarfile.TarError) as error:
        print(f"release manifest: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
