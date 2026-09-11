"""Registry freshness identifies content, independently of schema or time."""
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "registry-snapshot.py"
SPEC = importlib.util.spec_from_file_location("registry_snapshot", SCRIPT)
snapshot = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(snapshot)


class RegistrySnapshotTests(unittest.TestCase):
    def test_timestamp_and_self_stamp_do_not_change_identity(self):
        original = {"versions": [{"version": "1.0.0", "date": "2026-01-01"}], "updated_at": "old"}
        changed = {**original, "updated_at": "new", "snapshot_revision": "old-digest"}
        self.assertEqual(snapshot.snapshot_revision(original), snapshot.snapshot_revision(changed))
        changed["versions"] = [{"version": "1.0.0", "date": "2026-01-02"}]
        self.assertNotEqual(snapshot.snapshot_revision(original), snapshot.snapshot_revision(changed))

    def test_aggregate_binds_product_content_and_repeated_stamp_is_stable(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            product = root / "codex.json"
            index = root / "registry.json"
            product.write_text(json.dumps({"versions": [{"version": "1.0.0", "verified": {"linux": True}}]}))
            index.write_text(json.dumps({"products": [{"product": "codex", "latest": "1.0.0"}]}))
            snapshot.stamp_registry(root)
            first = index.read_text()
            product_first = json.loads(product.read_text())
            self.assertEqual(product_first["schema_version"], 1)
            self.assertEqual(json.loads(first)["products"][0]["snapshot_revision"], product_first["snapshot_revision"])
            snapshot.stamp_registry(root)
            self.assertEqual(index.read_text(), first)
            product_first["versions"][0]["verified"] = {"macos": True}
            product.write_text(json.dumps(product_first))
            snapshot.stamp_registry(root)
            self.assertNotEqual(json.loads(index.read_text())["snapshot_revision"], json.loads(first)["snapshot_revision"])
            self.assertEqual(json.loads(product.read_text())["schema_version"], 1)

    def test_missing_product_fails_before_stamping_any_file(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            index = root / "registry.json"
            text = json.dumps({"products": [{"product": "codex"}]})
            index.write_text(text)
            with self.assertRaises(FileNotFoundError):
                snapshot.stamp_registry(root)
            self.assertEqual(index.read_text(), text)

if __name__ == "__main__":
    unittest.main()
