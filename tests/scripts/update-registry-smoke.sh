#!/usr/bin/env bash
# Smoke test: scripts/update-registry.sh must run on bash (Ubuntu CI does not
# ship zsh). We invoke it for a single product to keep the network footprint
# small, then validate the JSON it wrote.
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo_root"

bash scripts/update-registry.sh claude

for f in docs/api/claude.json docs/api/registry.json; do
    test -f "$f" || { echo "missing $f" >&2; exit 1; }
done

python3 - <<'PY'
import json
with open("docs/api/claude.json") as f:
    claude = json.load(f)
assert claude["product"] == "claude", claude
assert claude["versions"], "no versions written"
with open("docs/api/registry.json") as f:
    index = json.load(f)
products = {p["product"] for p in index["products"]}
assert "claude" in products, index
print(f"update-registry-smoke: ok ({len(claude['versions'])} claude versions)")
PY
