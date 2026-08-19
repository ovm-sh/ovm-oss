#!/usr/bin/env bash
# Smoke test: scripts/update-registry.sh must run on bash (Ubuntu CI does not
# ship zsh). Exercise the Claude updater against a fixture npm so the test is
# deterministic and cannot modify the checkout's real docs/api registry.
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo_root"

# Every product in the default set must have a dispatch arm AND an updater.
#
# Static, because the alternative is a network round-trip per product. It closes
# a real hole: a product added to the default PRODUCTS array without a matching
# `case` arm prints "  Unknown product: <name>" and *continues*, so a full
# refresh silently skips it, `write_index` never sees a file for it, and the
# run still exits 0. This smoke previously invoked only claude, so it stayed
# green through exactly that.
python3 - <<'PY'
import re
import sys

script = open("scripts/update-registry.sh", encoding="utf-8").read()

defaults = re.search(r"^\s*PRODUCTS=\(([^)]*)\)", script, re.MULTILINE)
assert defaults, "could not find the default PRODUCTS array"
default_products = defaults.group(1).split()

arms = dict(re.findall(r"^\s*([A-Za-z0-9_]+)\)\s*(update_[A-Za-z0-9_]+)\s*;;", script, re.MULTILINE))
functions = set(re.findall(r"^(update_[A-Za-z0-9_]+)\(\)", script, re.MULTILINE))

problems = []
for product in default_products:
    if product not in arms:
        problems.append(f"{product}: in the default PRODUCTS array but has no dispatch case arm")
    elif arms[product] not in functions:
        problems.append(f"{product}: dispatches to {arms[product]}(), which is not defined")
for product, fn in arms.items():
    if product not in default_products:
        problems.append(f"{product}: has a dispatch arm but is not in the default PRODUCTS array")

if problems:
    for problem in problems:
        print(f"update-registry dispatch parity: {problem}", file=sys.stderr)
    raise SystemExit(1)

print(f"update-registry dispatch parity: ok ({', '.join(default_products)})")
PY

fixture_root=$(mktemp -d)
trap 'rm -rf "$fixture_root"' EXIT
fixture_repo="$fixture_root/repo"
mock_bin="$fixture_root/bin"
mock_python="$fixture_root/python"
mkdir -p "$fixture_repo/scripts" "$fixture_repo/docs/api" "$mock_bin" "$mock_python"
cp scripts/update-registry.sh "$fixture_repo/scripts/update-registry.sh"

cat > "$mock_bin/npm" <<'MOCK_NPM'
#!/usr/bin/env bash
set -euo pipefail

field=${3:-}
scenario=${MOCK_NPM_SCENARIO:-initial}

case "$scenario:$field" in
  initial:versions)
    printf '%s\n' '["0.1.0","0.1.1","0.1.2","0.1.3","0.1.4","0.1.5","0.1.6"]'
    ;;
  initial:time)
    printf '%s\n' '{"0.1.0":"2026-07-01T12:34:56.000Z","0.1.1":["2026-07-02"],"0.1.2":"temporarily unavailable","0.1.3":"2026-99-99T00:00:00Z","0.1.4":"2026-07-05","0.1.5":null}'
    ;;
  initial:dist-tags)
    printf '%s\n' '{"latest":"0.1.6","next":"0.1.5","stale":"9.9.9"}'
    ;;
  reduced:versions)
    printf '%s\n' '["0.1.6"]'
    ;;
  reduced:time)
    printf '%s\n' '{"0.1.6":"2026-07-07T00:00:00Z"}'
    ;;
  reduced:dist-tags)
    printf '%s\n' '{"latest":"0.1.6"}'
    ;;
  *)
    echo "unexpected mock npm request: scenario=$scenario field=$field" >&2
    exit 2
    ;;
esac
MOCK_NPM
chmod +x "$mock_bin/npm"

PATH="$mock_bin:$PATH" MOCK_NPM_SCENARIO=initial \
  bash "$fixture_repo/scripts/update-registry.sh" claude

for f in "$fixture_repo/docs/api/claude.json" "$fixture_repo/docs/api/registry.json"; do
  test -f "$f" || { echo "missing $f" >&2; exit 1; }
done

python3 - "$fixture_repo/docs/api" <<'PY'
import json
import sys

api_dir = sys.argv[1]
with open(f"{api_dir}/claude.json") as f:
    data = json.load(f)
assert data["product"] == "claude", data
assert [entry["version"] for entry in data["versions"]] == [
    "0.1.0", "0.1.1", "0.1.2", "0.1.3", "0.1.4", "0.1.5", "0.1.6"
], data
by_version = {entry["version"]: entry for entry in data["versions"]}
assert by_version["0.1.0"]["date"] == "2026-07-01", by_version
assert by_version["0.1.4"]["date"] == "2026-07-05", by_version
for version in ("0.1.1", "0.1.2", "0.1.3", "0.1.5", "0.1.6"):
    assert "date" not in by_version[version], by_version[version]
assert data["dist_tags"] == {"latest": "0.1.6", "next": "0.1.5", "stale": ""}, data
with open(f"{api_dir}/registry.json") as f:
    index = json.load(f)
products = {p["product"] for p in index["products"]}
assert products == {"claude"}, index
PY

# Seven published versions reduced to one means six new retirements. The
# established floor is five, so this must trip the breaker. A blanket token
# must not bypass it, and a failed refresh must leave the registry untouched.
cp "$fixture_repo/docs/api/claude.json" "$fixture_root/claude-before.json"
if PATH="$mock_bin:$PATH" MOCK_NPM_SCENARIO=reduced OVM_ALLOW_MASS_RETIREMENT=all \
  bash "$fixture_repo/scripts/update-registry.sh" claude >"$fixture_root/blanket.out" 2>&1; then
  echo "Claude mass-retirement blanket override unexpectedly succeeded" >&2
  exit 1
fi
grep -q 'OVM_ALLOW_MASS_RETIREMENT=all permits nothing' "$fixture_root/blanket.out"
cmp "$fixture_root/claude-before.json" "$fixture_repo/docs/api/claude.json"

# A product-named override is the deliberate escape hatch and records the six
# retired versions after the operator has reviewed this exact product.
PATH="$mock_bin:$PATH" MOCK_NPM_SCENARIO=reduced OVM_ALLOW_MASS_RETIREMENT=claude \
  bash "$fixture_repo/scripts/update-registry.sh" claude

python3 - "$fixture_repo/docs/api/claude.json" <<'PY'
import json
import sys

with open(sys.argv[1]) as f:
    data = json.load(f)
assert [entry["version"] for entry in data["versions"]] == ["0.1.6"], data
assert len(data["retired_versions"]) == 6, data
assert {entry["version"] for entry in data["retired_versions"]} == {
    "0.1.0", "0.1.1", "0.1.2", "0.1.3", "0.1.4", "0.1.5"
}, data
print("update-registry-smoke: ok (Claude fixtures, metadata, retirement guard)")
PY

# GitHub's releases endpoint returns 422 once a repository's pagination crosses
# the 1,000-result ceiling. That is a known incomplete listing, not a transient
# request failure and definitely not proof that older releases were deleted.
# Patch urllib only for this subprocess so the test exercises the real embedded
# Codex updater without a network round-trip.
cat > "$mock_python/sitecustomize.py" <<'PY'
import io
import json
import time
import urllib.error
import urllib.parse
import urllib.request


class Response:
    def __init__(self, payload):
        self.payload = json.dumps(payload).encode()

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return False

    def read(self):
        return self.payload


def urlopen(request, timeout=30):
    del timeout
    parsed = urllib.parse.urlparse(request.full_url)
    if "/releases/tags/" in parsed.path:
        tag = urllib.parse.unquote(parsed.path.rsplit("/", 1)[-1])
        if tag == "rust-v0.0.1":
            return Response({
                "tag_name": tag,
                "published_at": "2025-12-01T00:00:00Z",
                "assets": [{"name": "codex-aarch64-apple-darwin.tar.gz"}],
            })
        body = io.BytesIO(json.dumps({"message": "Not Found"}).encode())
        raise urllib.error.HTTPError(request.full_url, 404, "Not Found", {}, body)
    query = urllib.parse.parse_qs(parsed.query)
    page = int(query.get("page", ["1"])[0])
    if page == 1:
        return Response([
            {
                "tag_name": "rust-v0.2.0",
                "published_at": "2026-08-19T00:00:00Z",
                "assets": [{"name": "codex-aarch64-apple-darwin.tar.gz"}],
            },
            {
                "tag_name": "nightly-boundary",
                "published_at": "2026-01-15T00:00:00Z",
                "assets": [],
            },
        ])
    body = io.BytesIO(json.dumps({
        "message": "Only the first 1000 results are available."
    }).encode())
    raise urllib.error.HTTPError(
        request.full_url, 422, "Unprocessable Entity", {}, body
    )


urllib.request.urlopen = urlopen
time.sleep = lambda _seconds: None
PY

cat > "$fixture_repo/docs/api/codex.json" <<'JSON'
{
  "product": "codex",
  "display_name": "Codex",
  "source": "github:openai/codex",
  "updated_at": "2026-08-18T00:00:00Z",
  "dist_tags": {"latest": "rust-v0.1.0"},
  "versions": [
    {"version": "rust-v0.1.0", "date": "2026-01-01", "verified": {"macos-auth": true}},
    {"version": "rust-v9.0.0", "date": "2026-08-18"}
  ],
  "retired_versions": [
    {"version": "rust-v0.0.1", "date": "2025-12-01", "last_seen_at": "2026-01-01T00:00:00Z", "retired_at": "2026-01-02T00:00:00Z"}
  ]
}
JSON

PYTHONPATH="$mock_python" bash "$fixture_repo/scripts/update-registry.sh" codex \
  >"$fixture_root/codex.out" 2>&1
grep -q 'GitHub releases listing capped at 1000 results' "$fixture_root/codex.out"

python3 - "$fixture_repo/docs/api/codex.json" <<'PY'
import json
import sys

with open(sys.argv[1]) as f:
    data = json.load(f)
assert [entry["version"] for entry in data["versions"]] == [
    "rust-v0.0.1", "rust-v0.1.0", "rust-v0.2.0"
], data
assert [entry["version"] for entry in data["retired_versions"]] == [
    "rust-v9.0.0"
], data
assert data["dist_tags"] == {"latest": "rust-v0.2.0"}, data
assert data["versions"][1]["verified"] == {"macos-auth": True}, data
print("update-registry-smoke: ok (Codex 1000-release cap preserves known history)")
PY
