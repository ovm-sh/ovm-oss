"""Release identity must survive mirrors and reject mixed candidate bytes."""

import importlib.util
import io
import json
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("release_manifest", ROOT / "scripts/release-manifest.py")
manifest = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = manifest
spec.loader.exec_module(manifest)


class ReleaseManifestTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.source = self.root / "source"
        self.artifacts = self.root / "artifacts"
        self.artifacts.mkdir()
        (self.source / "crates/ovm").mkdir(parents=True)
        self.bundle = b"ovm-bundle-v1\nmain\tovm\tovm\nside\tovm-claudex\tovm-claudex\n"
        (self.source / "crates/ovm/ovm-bundle-v1.tsv").write_bytes(self.bundle)
        (self.source / "install.sh").write_text("#!/bin/sh\necho installer\n")
        for target in manifest.TARGETS:
            self.archive(target)

    def archive(self, target, extra=None):
        archive = self.artifacts / f"ovm-{target}.tar.gz"
        with tarfile.open(archive, "w:gz") as stream:
            files = {"ovm-bundle-v1.tsv": self.bundle, "ovm": b"core", "ovm-claudex": b"plugin"}
            if extra:
                files.update(extra)
            for name, data in files.items():
                member = tarfile.TarInfo(name)
                member.size = len(data)
                member.mode = 0o755
                stream.addfile(member, io.BytesIO(data))
        (self.artifacts / (archive.name + ".sha256")).write_text(
            manifest.file_digest(archive) + "  " + archive.name + "\n"
        )
        return archive

    def create(self, **kwargs):
        return manifest.create("v0.1.8-alpha.3", "a" * 40, "public",
                               self.artifacts, self.source, **kwargs)

    def test_identity_is_content_based_and_carries_each_binary(self):
        first = self.create()
        archive = self.artifacts / first["artifacts"][0]["name"]
        archive.touch()
        self.assertEqual(first, self.create())
        self.assertEqual(first["artifacts"][0]["binaries"][1]["sha256"], manifest.digest(b"plugin"))
        self.assertNotIn(str(self.root), json.dumps(first))

    def test_modified_archive_cannot_reuse_old_checksum(self):
        archive = self.artifacts / f"ovm-{manifest.TARGETS[0]}.tar.gz"
        archive.write_bytes(archive.read_bytes() + b"changed")
        with self.assertRaisesRegex(ValueError, "checksum"):
            self.create()

    def test_matching_checksum_does_not_admit_extra_archive_content(self):
        self.archive(manifest.TARGETS[0], {"surprise": b"extra"})
        with self.assertRaisesRegex(ValueError, "bundle contents"):
            self.create()

    def test_missing_platform_does_not_produce_partial_release_manifest(self):
        (self.artifacts / f"ovm-{manifest.TARGETS[0]}.tar.gz").unlink()
        with self.assertRaisesRegex(ValueError, "exactly one"):
            self.create()

    def test_private_and_public_builds_have_distinct_identity(self):
        public = self.create()
        private = manifest.create("v0.1.8-alpha.3", "a" * 40, "qualification", self.artifacts, self.source)
        self.assertNotEqual(public["revision"], private["revision"])

    def media(self):
        release = self.create()
        (self.source / "site/media").mkdir(parents=True)
        assets = {}
        for key, name in (("output", "clip.mp4"), ("poster", "poster.png")):
            path = self.source / "site/media" / name
            path.write_bytes(name.encode())
            assets[key] = {"path": f"site/media/{name}", "sha256": manifest.file_digest(path)}
        candidate = {"tag": release["release_tag"], "source_commit": "a" * 40,
                     "target": release["artifacts"][0]["target"],
                     "archive_sha256": release["artifacts"][0]["sha256"],
                     "installer_sha256": release["installer_sha256"]}
        path = self.source / "site/media/clip.mp4.json"
        document = {"schema_version": 1, "kind": "ovm-media", "release_tag": release["release_tag"],
                    "candidate": candidate, **assets}
        path.write_text(json.dumps(document))
        return path, document

    def test_media_binds_matching_candidate_and_final_output(self):
        path, _ = self.media()
        release = self.create(media_paths=[path])
        self.assertEqual(len(release["media"]), 1)
        self.assertEqual(release["media"][0]["manifest"], "site/media/clip.mp4.json")
        (self.source / "site/media/clip.mp4").write_bytes(b"stale take")
        with self.assertRaisesRegex(ValueError, "changed after recording"):
            self.create(media_paths=[path])

    def test_media_cannot_mix_private_or_other_candidate_artifacts(self):
        path, document = self.media()
        document["candidate"]["archive_sha256"] = "0" * 64
        path.write_text(json.dumps(document))
        with self.assertRaisesRegex(ValueError, "this release's archive"):
            self.create(media_paths=[path])

    def test_media_cannot_publish_host_paths(self):
        path, document = self.media()
        document["output"]["path"] = "/private/clip.mp4"
        path.write_text(json.dumps(document))
        with self.assertRaisesRegex(ValueError, "relative"):
            self.create(media_paths=[path])


if __name__ == "__main__":
    unittest.main()
